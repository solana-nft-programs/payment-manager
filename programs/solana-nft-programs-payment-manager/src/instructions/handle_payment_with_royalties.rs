use mpl_token_metadata::accounts::Metadata;
use mpl_utils::assert_derivation;

use {
    crate::{errors::ErrorCode, state::*},
    anchor_lang::prelude::*,
    anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer},
};

#[derive(Accounts)]
#[instruction(amount: u64)]
pub struct HandlePaymentWithRoyaltiesCtx<'info> {
    #[account(mut)]
    payment_manager: Box<Account<'info, PaymentManager>>,

    #[account(mut)]
    payer_token_account: Box<Account<'info, TokenAccount>>,
    #[account(mut, constraint = fee_collector_token_account.owner == payment_manager.fee_collector @ ErrorCode::InvalidFeeCollectorTokenAccount)]
    fee_collector_token_account: Box<Account<'info, TokenAccount>>,
    #[account(mut)]
    payment_token_account: Box<Account<'info, TokenAccount>>,

    payment_mint: Box<Account<'info, Mint>>,
    mint: Box<Account<'info, Mint>>,
    /// CHECK: This is not dangerous because we don't read or write from this account
    mint_metadata: AccountInfo<'info>,

    payer: Signer<'info>,
    token_program: Program<'info, Token>,
    // > Remaining accounts for each mint creator
    // creator token account
}

pub fn handler<'info>(ctx: Context<'_, '_, '_, 'info, HandlePaymentWithRoyaltiesCtx<'info>>, payment_amount: u64) -> Result<()> {
    let payment_manager = &mut ctx.accounts.payment_manager;
    // maker-taker fees
    let maker_fee = payment_amount
        .checked_mul(payment_manager.maker_fee_basis_points.into())
        .expect("Multiplication error")
        .checked_div(BASIS_POINTS_DIVISOR.into())
        .expect("Division error");
    let taker_fee = payment_amount
        .checked_mul(payment_manager.taker_fee_basis_points.into())
        .expect("Multiplication error")
        .checked_div(BASIS_POINTS_DIVISOR.into())
        .expect("Division error");
    let mut total_fees = maker_fee.checked_add(taker_fee).expect("Add error");

    // assert metadata account derivation
    assert_derivation(
        &mpl_token_metadata::ID,
        &ctx.accounts.mint_metadata.to_account_info(),
        &["metadata".to_string().as_bytes(), mpl_token_metadata::ID.as_ref(), ctx.accounts.mint.key().as_ref()],
        error!(ErrorCode::InvalidMintMetadataOwner),
    )?;

    // royalties
    let mut fees_paid_out: u64 = 0;
    let remaining_accs = &mut ctx.remaining_accounts.iter();
    if !ctx.accounts.mint_metadata.data_is_empty() {
        if ctx.accounts.mint_metadata.to_account_info().owner.key() != mpl_token_metadata::ID {
            return Err(error!(ErrorCode::InvalidMintMetadataOwner));
        }
        let mint_metadata_data = ctx.accounts.mint_metadata.try_borrow_mut_data().expect("Failed to borrow data");
        let mint_metadata = Metadata::deserialize(&mut mint_metadata_data.as_ref()).expect("Failed to deserialize metadata");
        if mint_metadata.mint != ctx.accounts.mint.key() {
            return Err(error!(ErrorCode::InvalidMintMetadata));
        }
        let seller_fee = if payment_manager.include_seller_fee_basis_points {
            payment_amount
                .checked_mul(mint_metadata.seller_fee_basis_points.into())
                .expect("Multiplication error")
                .checked_div(BASIS_POINTS_DIVISOR.into())
                .expect("Division error")
        } else {
            0
        };
        let total_creators_fee = total_fees
            .checked_mul(payment_manager.royalty_fee_share.unwrap_or(DEFAULT_ROYALTY_FEE_SHARE))
            .unwrap()
            .checked_div(BASIS_POINTS_DIVISOR.into())
            .expect("Div error")
            .checked_add(seller_fee)
            .expect("Add error");
        total_fees = total_fees.checked_add(seller_fee).expect("Add error");

        if let Some(creators) = mint_metadata.creators {
            let creator_amounts: Vec<u64> = creators
                .clone()
                .into_iter()
                .map(|creator| total_creators_fee.checked_mul(u64::try_from(creator.share).expect("Could not cast u8 to u64")).unwrap())
                .collect();
            let creator_amounts_sum: u64 = creator_amounts.iter().sum();
            let mut creators_fee_remainder = total_creators_fee.checked_sub(creator_amounts_sum.checked_div(100).expect("Div error")).expect("Sub error");
            for creator in creators {
                if creator.share != 0 {
                    let creator_token_account_info = next_account_info(remaining_accs)?;
                    let creator_token_account = Account::<TokenAccount>::try_from(creator_token_account_info)?;
                    if creator_token_account.owner != creator.address || creator_token_account.mint != ctx.accounts.payment_mint.key() {
                        return Err(error!(ErrorCode::InvalidTokenAccount));
                    }
                    let share = u64::try_from(creator.share).expect("Could not cast u8 to u64");
                    let creator_fee_remainder_amount = u64::from(creators_fee_remainder > 0);
                    let creator_fee_amount = total_creators_fee
                        .checked_mul(share)
                        .unwrap()
                        .checked_div(100)
                        .expect("Div error")
                        .checked_add(creator_fee_remainder_amount)
                        .expect("Add error");
                    creators_fee_remainder = creators_fee_remainder.checked_sub(creator_fee_remainder_amount).expect("Sub error");

                    if creator_fee_amount > 0 {
                        fees_paid_out = fees_paid_out.checked_add(creator_fee_amount).expect("Add error");
                        let cpi_accounts = Transfer {
                            from: ctx.accounts.payer_token_account.to_account_info(),
                            to: creator_token_account_info.to_account_info(),
                            authority: ctx.accounts.payer.to_account_info(),
                        };
                        let cpi_program = ctx.accounts.token_program.to_account_info();
                        let cpi_context = CpiContext::new(cpi_program, cpi_accounts);
                        token::transfer(cpi_context, creator_fee_amount)?;
                    }
                }
            }
        }
    }

    // calculate fees
    let buy_side_fee = payment_amount
        .checked_mul(DEFAULT_BUY_SIDE_FEE_SHARE)
        .unwrap()
        .checked_div(BASIS_POINTS_DIVISOR.into())
        .expect("Div error");
    let mut fee_collector_fee = total_fees.checked_add(buy_side_fee).expect("Add error").checked_sub(fees_paid_out).expect("Sub error");

    // pay buy side fee
    let buy_side_token_account_info = next_account_info(remaining_accs);
    if buy_side_token_account_info.is_ok() {
        let buy_side_token_account = Account::<TokenAccount>::try_from(buy_side_token_account_info?);
        if buy_side_token_account.is_ok() {
            let cpi_accounts = Transfer {
                from: ctx.accounts.payer_token_account.to_account_info(),
                to: buy_side_token_account?.to_account_info(),
                authority: ctx.accounts.payer.to_account_info(),
            };
            let cpi_program = ctx.accounts.token_program.to_account_info();
            let cpi_context = CpiContext::new(cpi_program, cpi_accounts);
            token::transfer(cpi_context, buy_side_fee)?;

            // remove buy side fee out of fee collector fee
            fee_collector_fee = fee_collector_fee.checked_sub(buy_side_fee).expect("Sub error");
        }
    }

    if fee_collector_fee > 0 {
        // pay remaining fees to fee_colector
        let cpi_accounts = Transfer {
            from: ctx.accounts.payer_token_account.to_account_info(),
            to: ctx.accounts.fee_collector_token_account.to_account_info(),
            authority: ctx.accounts.payer.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        let cpi_context = CpiContext::new(cpi_program, cpi_accounts);
        token::transfer(cpi_context, fee_collector_fee)?;
    }

    // pay target
    let cpi_accounts = Transfer {
        from: ctx.accounts.payer_token_account.to_account_info(),
        to: ctx.accounts.payment_token_account.to_account_info(),
        authority: ctx.accounts.payer.to_account_info(),
    };
    let cpi_program = ctx.accounts.token_program.to_account_info();
    let cpi_context = CpiContext::new(cpi_program, cpi_accounts);
    token::transfer(
        cpi_context,
        payment_amount
            .checked_add(taker_fee)
            .expect("Add error")
            .checked_sub(total_fees)
            .expect("Sub error")
            .checked_sub(buy_side_fee)
            .expect("Sub error"),
    )?;

    Ok(())
}
