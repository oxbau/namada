//! A tx for a delegator (non-validator bond owner) to redelegate bonded tokens
//! from one validator to another.

use namada_tx_prelude::*;

#[transaction(gas = 2453242)]
fn apply_tx(ctx: &mut Ctx, tx_data: Tx) -> TxResult {
    let signed = tx_data;
    let data = signed.data().ok_or_err_msg("Missing data").map_err(|err| {
        ctx.set_commitment_sentinel();
        err
    })?;
    let transaction::pos::Redelegation {
        src_validator,
        dest_validator,
        owner,
        amount,
    } = transaction::pos::Redelegation::try_from_slice(&data[..])
        .wrap_err("failed to decode a Redelegation")?;
    ctx.redelegate_tokens(&owner, &src_validator, &dest_validator, amount)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use namada::ledger::pos::{OwnedPosParams, PosVP};
    use namada::proof_of_stake::types::{GenesisValidator, WeightedValidator};
    use namada::proof_of_stake::{
        bond_handle, read_consensus_validator_set_addresses_with_stake,
        read_total_stake, read_validator_stake, unbond_handle,
    };
    use namada::types::dec::Dec;
    use namada::types::storage::Epoch;
    use namada_tests::log::test;
    use namada_tests::native_vp::pos::init_pos;
    use namada_tests::native_vp::TestNativeVpEnv;
    use namada_tests::tx::*;
    use namada_tx_prelude::address::InternalAddress;
    use namada_tx_prelude::borsh_ext::BorshSerializeExt;
    use namada_tx_prelude::chain::ChainId;
    use namada_tx_prelude::key::testing::arb_common_keypair;
    use namada_tx_prelude::key::RefTo;
    use namada_tx_prelude::proof_of_stake::parameters::testing::arb_pos_params;
    use namada_tx_prelude::token;
    use proptest::prelude::*;

    use super::*;

    proptest! {
        /// In this test we setup the ledger and PoS system with an arbitrary
        /// initial state with 1 genesis validator, a delegation bond if the
        /// unbond is for a delegation, arbitrary PoS parameters, and
        /// we generate an arbitrary unbond that we'd like to apply.
        ///
        /// After we apply the unbond, we check that all the storage values
        /// in PoS system have been updated as expected and then we also check
        /// that this transaction is accepted by the PoS validity predicate.
        #[test]
        fn test_tx_redelegate(
        (initial_stake, redelegation) in arb_initial_stake_and_redelegation(),
        // A key to sign the transaction
        key in arb_common_keypair(),
        pos_params in arb_pos_params(None)) {
            test_tx_redelegate_aux(initial_stake, redelegation, key, pos_params).unwrap()
        }
    }

    // TODO: more assertions needed!!
    fn test_tx_redelegate_aux(
        initial_stake: token::Amount,
        redelegation: transaction::pos::Redelegation,
        key: key::common::SecretKey,
        pos_params: OwnedPosParams,
    ) -> TxResult {
        // Remove the validator stake threshold for simplicity
        let pos_params = OwnedPosParams {
            validator_stake_threshold: token::Amount::zero(),
            ..pos_params
        };
        dbg!(&initial_stake, &redelegation);

        let consensus_key_1 = key::testing::keypair_1().ref_to();
        let consensus_key_2 = key::testing::keypair_2().ref_to();
        let protocol_key = key::testing::keypair_2().ref_to();
        let eth_cold_key = key::testing::keypair_3().ref_to();
        let eth_hot_key = key::testing::keypair_4().ref_to();
        let commission_rate = Dec::new(5, 2).expect("Cannot fail");
        let max_commission_rate_change = Dec::new(1, 2).expect("Cannot fail");

        let genesis_validators = [
            GenesisValidator {
                address: redelegation.src_validator.clone(),
                tokens: token::Amount::zero(),
                consensus_key: consensus_key_1,
                protocol_key: protocol_key.clone(),
                eth_cold_key: eth_cold_key.clone(),
                eth_hot_key: eth_hot_key.clone(),
                commission_rate,
                max_commission_rate_change,
                metadata: Default::default(),
            },
            GenesisValidator {
                address: redelegation.dest_validator.clone(),
                tokens: token::Amount::zero(),
                consensus_key: consensus_key_2,
                protocol_key,
                eth_cold_key,
                eth_hot_key,
                commission_rate,
                max_commission_rate_change,
                metadata: Default::default(),
            },
        ];

        let pos_params =
            init_pos(&genesis_validators[..], &pos_params, Epoch(0));

        let native_token = tx_host_env::with(|tx_env| {
            let native_token = tx_env.wl_storage.storage.native_token.clone();
            let owner = &redelegation.owner;
            tx_env.spawn_accounts([owner]);

            // First, credit the delegator with the initial stake,
            // before we initialize the bond below
            tx_env.credit_tokens(owner, &native_token, initial_stake);
            native_token
        });

        // Create the initial bond.
        ctx().bond_tokens(
            Some(&redelegation.owner),
            &redelegation.src_validator,
            initial_stake,
        )?;
        tx_host_env::commit_tx_and_block();

        let tx_code = vec![];
        let tx_data = redelegation.serialize_to_vec();
        let mut tx = Tx::new(ChainId::default(), None);
        tx.add_code(tx_code, None)
            .add_serialized_data(tx_data)
            .sign_wrapper(key);
        let signed_tx = tx;

        // Check that PoS balance is the same as the initial validator stake
        let pos_balance_key = token::balance_key(
            &native_token,
            &Address::Internal(InternalAddress::PoS),
        );
        let pos_balance_pre: token::Amount = ctx()
            .read(&pos_balance_key)?
            .expect("PoS must have balance");
        assert_eq!(pos_balance_pre, initial_stake);

        let mut epoched_total_stake_pre: Vec<token::Amount> = Vec::new();
        let mut epoched_src_validator_stake_pre: Vec<token::Amount> =
            Vec::new();
        let mut epoched_dest_validator_stake_pre: Vec<token::Amount> =
            Vec::new();
        let mut epoched_src_bonds_pre: Vec<Option<token::Amount>> = Vec::new();
        let mut epoched_dest_bonds_pre: Vec<Option<token::Amount>> = Vec::new();
        let mut epoched_validator_set_pre: Vec<BTreeSet<WeightedValidator>> =
            Vec::new();

        for epoch in 0..=pos_params.withdrawable_epoch_offset() {
            epoched_total_stake_pre.push(read_total_stake(
                ctx(),
                &pos_params,
                Epoch(epoch),
            )?);
            epoched_src_validator_stake_pre.push(read_validator_stake(
                ctx(),
                &pos_params,
                &redelegation.src_validator,
                Epoch(epoch),
            )?);
            epoched_dest_validator_stake_pre.push(read_validator_stake(
                ctx(),
                &pos_params,
                &redelegation.dest_validator,
                Epoch(epoch),
            )?);
            epoched_src_bonds_pre.push(
                bond_handle(&redelegation.owner, &redelegation.src_validator)
                    .get_delta_val(ctx(), Epoch(epoch))?,
            );
            epoched_dest_bonds_pre.push(
                bond_handle(&redelegation.owner, &redelegation.src_validator)
                    .get_delta_val(ctx(), Epoch(epoch))?,
            );
            epoched_validator_set_pre.push(
                read_consensus_validator_set_addresses_with_stake(
                    ctx(),
                    Epoch(epoch),
                )?,
            );
        }

        // Apply the redelegation tx
        apply_tx(ctx(), signed_tx)?;

        // Read the data after the redelegation tx is executed.
        // The following storage keys should be updated:
        //     - `#{PoS}/validator/#{validator}/deltas`
        //     - `#{PoS}/total_deltas`
        //     - `#{PoS}/validator_set`

        let mut epoched_src_bonds_post: Vec<Option<token::Amount>> = Vec::new();
        let mut epoched_dest_bonds_post: Vec<Option<token::Amount>> =
            Vec::new();
        for epoch in 0..=pos_params.unbonding_len {
            epoched_src_bonds_post.push(
                bond_handle(&redelegation.owner, &redelegation.src_validator)
                    .get_delta_val(ctx(), Epoch(epoch))?,
            );
            epoched_dest_bonds_post.push(
                bond_handle(&redelegation.owner, &redelegation.dest_validator)
                    .get_delta_val(ctx(), Epoch(epoch))?,
            );
        }

        // Before pipeline offset, there can only be self-bond for genesis
        // validator. In case of a delegation the state is setup so that there
        // is no bond until pipeline offset.
        for epoch in 0..pos_params.pipeline_len {
            assert_eq!(
                read_validator_stake(
                    ctx(),
                    &pos_params,
                    &redelegation.src_validator,
                    Epoch(epoch)
                )?,
                token::Amount::zero(),
                "The validator stake before the pipeline offset must be 0 - \
                 checking in epoch: {epoch}"
            );
            assert_eq!(
                read_validator_stake(
                    ctx(),
                    &pos_params,
                    &redelegation.dest_validator,
                    Epoch(epoch)
                )?,
                token::Amount::zero(),
                "The validator stake before the pipeline offset must be 0 - \
                 checking in epoch: {epoch}"
            );
            assert_eq!(
                read_total_stake(ctx(), &pos_params, Epoch(epoch))?,
                token::Amount::zero(),
                "The total stake before the pipeline offset must be 0 - \
                 checking in epoch: {epoch}"
            );
            assert_eq!(
                epoched_validator_set_pre[epoch as usize],
                read_consensus_validator_set_addresses_with_stake(
                    ctx(),
                    Epoch(epoch),
                )?,
                "Validator set before pipeline offset must not change - \
                 checking epoch {epoch}"
            );
        }

        // Check stakes after the pipeline length
        for epoch in
            pos_params.pipeline_len..=pos_params.withdrawable_epoch_offset()
        {
            assert_eq!(
                read_validator_stake(
                    ctx(),
                    &pos_params,
                    &redelegation.src_validator,
                    Epoch(epoch)
                )?,
                initial_stake - redelegation.amount,
                "The validator stake at and after the pipeline offset must \
                 have changed - checking in epoch: {epoch}"
            );
            assert_eq!(
                read_validator_stake(
                    ctx(),
                    &pos_params,
                    &redelegation.dest_validator,
                    Epoch(epoch)
                )?,
                redelegation.amount,
                "The validator stake at and after the pipeline offset must \
                 have changed - checking in epoch: {epoch}"
            );
            assert_eq!(
                read_total_stake(ctx(), &pos_params, Epoch(epoch))?,
                initial_stake,
                "The total stake at and after the pipeline offset must have \
                 changed - checking in epoch: {epoch}"
            );
        }
        // Check validator sets
        assert_eq!(
            BTreeSet::from_iter([
                WeightedValidator {
                    bonded_stake: initial_stake - redelegation.amount,
                    address: redelegation.src_validator.clone()
                },
                WeightedValidator {
                    bonded_stake: redelegation.amount,
                    address: redelegation.dest_validator.clone()
                }
            ]),
            read_consensus_validator_set_addresses_with_stake(
                ctx(),
                Epoch(pos_params.pipeline_len),
            )?,
            "The validator set at pipeline offset should have changed"
        );

        // Check that PoS account balance is unchanged by the redelegation
        let pos_balance_post: token::Amount =
            ctx().read(&pos_balance_key)?.unwrap();
        assert_eq!(
            pos_balance_pre, pos_balance_post,
            "Unbonding should not affect PoS system balance"
        );

        // Check that no unbonds exist
        assert!(
            unbond_handle(&redelegation.owner, &redelegation.src_validator)
                .is_empty(ctx())?
        );
        assert!(
            unbond_handle(&redelegation.owner, &redelegation.dest_validator)
                .is_empty(ctx())?
        );

        // Check bonds
        for epoch in 0..pos_params.withdrawable_epoch_offset() {
            let (exp_src_bond, exp_dest_bond) =
                if epoch == pos_params.pipeline_len {
                    (
                        Some(initial_stake - redelegation.amount),
                        Some(redelegation.amount),
                    )
                } else {
                    (None, None)
                };

            assert_eq!(
                bond_handle(&redelegation.owner, &redelegation.src_validator)
                    .get_delta_val(ctx(), Epoch(epoch))?,
                exp_src_bond,
                "After the tx is applied, the bond should be changed in \
                 place, checking epoch {epoch}"
            );
            assert_eq!(
                bond_handle(&redelegation.owner, &redelegation.dest_validator)
                    .get_delta_val(ctx(), Epoch(epoch))?,
                exp_dest_bond,
                "After the tx is applied, the bond should be changed in \
                 place, checking epoch {epoch}"
            );
        }

        // Use the tx_env to run PoS VP
        let tx_env = tx_host_env::take();
        let vp_env = TestNativeVpEnv::from_tx_env(tx_env, address::POS);
        let result = vp_env.validate_tx(PosVP::new);
        let result =
            result.expect("Validation of valid changes must not fail!");
        assert!(
            result,
            "PoS Validity predicate must accept this transaction"
        );
        Ok(())
    }

    /// Generates an initial validator stake and a redelegation, while making
    /// sure that the `initial_stake >= redelegation.amount`.
    fn arb_initial_stake_and_redelegation()
    -> impl Strategy<Value = (token::Amount, transaction::pos::Redelegation)>
    {
        // Generate initial stake
        token::testing::arb_amount_ceiled((i64::MAX / 8) as u64).prop_flat_map(
            |initial_stake| {
                // Use the initial stake to limit the bond amount
                let redelegation = arb_redelegation(
                    u128::try_from(initial_stake).unwrap() as u64,
                );
                // Use the generated initial stake too too
                (Just(initial_stake), redelegation)
            },
        )
    }

    /// Generates an arbitrary redelegation, with the amount constrained from
    /// above.
    fn arb_redelegation(
        max_amount: u64,
    ) -> impl Strategy<Value = transaction::pos::Redelegation> {
        (
            address::testing::arb_established_address(),
            address::testing::arb_established_address(),
            address::testing::arb_non_internal_address(),
            token::testing::arb_amount_non_zero_ceiled(max_amount),
        )
            .prop_map(
                |(src_validator, dest_validator, owner, amount)| {
                    let src_validator = Address::Established(src_validator);
                    let dest_validator = Address::Established(dest_validator);
                    transaction::pos::Redelegation {
                        src_validator,
                        dest_validator,
                        owner,
                        amount,
                    }
                },
            )
    }
}
