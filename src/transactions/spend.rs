use crate::{
    error::*,
    scripts::*,
    transactions::{
        utils, CpfpableTransaction, RevaultTransaction, INSANE_FEES, MAX_STANDARD_TX_WEIGHT,
        TX_VERSION,
    },
    txins::*,
    txouts::*,
};

use miniscript::{
    bitcoin::{
        blockdata::constants::max_money,
        consensus::encode::Decodable,
        util::psbt::{
            Global as PsbtGlobal, Input as PsbtIn, Output as PsbtOut,
            PartiallySignedTransaction as Psbt,
        },
        Amount, Network, OutPoint, SigHashType, Transaction,
    },
    DescriptorTrait,
};

#[cfg(feature = "use-serde")]
use {
    serde::de::{self, Deserialize, Deserializer},
    serde::ser::{Serialize, Serializer},
};

use std::{
    collections::{BTreeMap, HashSet},
    convert::TryInto,
};

impl_revault_transaction!(
    SpendTransaction,
    doc = "The transaction spending the unvaulting transaction, paying to one or multiple \
    externally-controlled addresses, and possibly to a new deposit txo for the change."
);
impl SpendTransaction {
    /// A spend transaction can batch multiple unvault txouts, and may have any number of
    /// txouts (destination and change) in addition to the CPFP one..
    ///
    /// The insane fees check is gated behind the `insane_fee_checks` parameter as the caller
    /// may want to create a transaction without a change output.
    ///
    /// BIP174 Creator and Updater roles.
    pub fn new(
        unvault_inputs: Vec<UnvaultTxIn>,
        spend_txouts: Vec<SpendTxOut>,
        change_txout: Option<DepositTxOut>,
        cpfp_descriptor: &DerivedCpfpDescriptor,
        lock_time: u32,
        insane_fee_check: bool,
    ) -> Result<SpendTransaction, TransactionCreationError> {
        // Check for duplicated inputs
        let uniq_txins: HashSet<OutPoint> = unvault_inputs.iter().map(|i| i.outpoint()).collect();
        if uniq_txins.len() != unvault_inputs.len() {
            return Err(TransactionCreationError::DuplicatedInput);
        }

        // The CPFP is tricky to compute. We could be smart and avoid some allocations here
        // but at the cost of clarity.
        let cpfp_txo = SpendTransaction::cpfp_txout(
            unvault_inputs.clone(),
            spend_txouts.clone(),
            change_txout.clone(),
            cpfp_descriptor,
            lock_time,
        );

        // Used later to check the maximum transaction size.
        let sat_weight = unvault_inputs
            .iter()
            .map(|txin| txin.txout().max_sat_weight())
            .sum::<usize>();

        // Record the value spent and sent
        let mut value_in: u64 = 0;
        let mut value_out: u64 = 0;

        let mut txos = if change_txout.is_some() {
            Vec::with_capacity(spend_txouts.len() + 2)
        } else {
            Vec::with_capacity(spend_txouts.len() + 1)
        };
        let mut psbtouts = Vec::with_capacity(txos.len());
        txos.push(cpfp_txo.txout().clone());
        psbtouts.push(PsbtOut {
            bip32_derivation: cpfp_txo.bip32_derivation().clone(),
            ..PsbtOut::default()
        });
        for spend_txout in spend_txouts.into_iter() {
            let txo = spend_txout.into_txout();

            if txo.value < txo.script_pubkey.dust_value().as_sat() {
                return Err(TransactionCreationError::Dust);
            }

            value_out += txo.value;

            txos.push(txo);
            psbtouts.push(PsbtOut::default());
        }

        if let Some(change_txout) = change_txout {
            let psbtout = PsbtOut {
                bip32_derivation: change_txout.bip32_derivation().clone(),
                ..PsbtOut::default()
            };

            let txo = change_txout.into_txout();

            if txo.value < txo.script_pubkey.dust_value().as_sat() {
                return Err(TransactionCreationError::Dust);
            }

            value_out += txo.value;

            txos.push(txo);
            psbtouts.push(psbtout);
        }

        let psbt = Psbt {
            global: PsbtGlobal {
                unsigned_tx: Transaction {
                    version: TX_VERSION,
                    lock_time,
                    input: unvault_inputs
                        .iter()
                        .map(|input| input.unsigned_txin())
                        .collect(),
                    output: txos,
                },
                version: 0,
                xpub: BTreeMap::new(),
                proprietary: BTreeMap::new(),
                unknown: BTreeMap::new(),
            },
            inputs: unvault_inputs
                .into_iter()
                .map(|input| {
                    let prev_txout = input.into_txout();
                    value_in += prev_txout.txout().value;
                    PsbtIn {
                        witness_script: Some(prev_txout.witness_script().clone()),
                        bip32_derivation: prev_txout.bip32_derivation().clone(),
                        sighash_type: Some(SigHashType::All), // Unvault spends are always signed with ALL
                        witness_utxo: Some(prev_txout.into_txout()),
                        ..PsbtIn::default()
                    }
                })
                .collect(),
            outputs: psbtouts,
        };

        // Make sure we didn't create a Monster Tx :tm: ..
        let unsigned_tx = &psbt.global.unsigned_tx;
        let witstrip_weight = unsigned_tx.get_weight();
        let total_weight = sat_weight
            .checked_add(witstrip_weight)
            .expect("Weight computation bug: cannot overflow");
        if total_weight > MAX_STANDARD_TX_WEIGHT as usize {
            return Err(TransactionCreationError::TooLarge);
        }

        if value_out > max_money(Network::Bitcoin) {
            return Err(TransactionCreationError::InsaneAmounts);
        }
        let fees = value_in
            .checked_sub(value_out)
            .ok_or(TransactionCreationError::NegativeFees)?;
        if insane_fee_check && fees > INSANE_FEES {
            return Err(TransactionCreationError::InsaneFees);
        }

        Ok(SpendTransaction(psbt))
    }

    /// Get the CPFP transaction output for a Spend transaction spending these `unvault_inputs`
    /// and creating these `spend_txouts`.
    ///
    /// The CPFP output value is dependant on the transaction size, see [practical-revaul
    /// t](https://github.com/revault/practical-revault/blob/master/transactions.md#spend_tx) for
    /// more details.
    pub fn cpfp_txout(
        unvault_inputs: Vec<UnvaultTxIn>,
        spend_txouts: Vec<SpendTxOut>,
        change_txout: Option<DepositTxOut>,
        cpfp_descriptor: &DerivedCpfpDescriptor,
        lock_time: u32,
    ) -> CpfpTxOut {
        let mut txos = Vec::with_capacity(spend_txouts.len() + 1);
        let dummy_cpfp_txo = CpfpTxOut::new(Amount::from_sat(u64::MAX), &cpfp_descriptor);
        txos.push(dummy_cpfp_txo.txout().clone());
        txos.extend(
            spend_txouts
                .into_iter()
                .map(|spend_txout| spend_txout.into_txout()),
        );

        if let Some(change_txout) = change_txout {
            txos.push(change_txout.into_txout());
        }

        let dummy_tx = Transaction {
            version: TX_VERSION,
            lock_time,
            input: unvault_inputs
                .iter()
                .map(|input| input.unsigned_txin())
                .collect(),
            output: txos,
        };

        let sat_weight: u64 = unvault_inputs
            .iter()
            .map(|txin| txin.txout().max_sat_weight())
            .sum::<usize>()
            .try_into()
            .expect("An usize doesn't fit in an u64?");
        let witstrip_weight: u64 = dummy_tx
            .get_weight()
            .try_into()
            .expect("Bug: an usize that doesn't fit in a u64?");
        let total_weight = sat_weight
            .checked_add(witstrip_weight)
            .expect("Weight computation bug: cannot overflow");

        // See https://github.com/revault/practical-revault/blob/master/transactions.md#spend_tx
        // for this arbirtrary value.
        let cpfp_value = 16 * total_weight;
        CpfpTxOut::new(Amount::from_sat(cpfp_value), &cpfp_descriptor)
    }

    // FIXME: feerate sanity checks
    /// Parse a Spend transaction from a PSBT
    pub fn from_raw_psbt(raw_psbt: &[u8]) -> Result<Self, TransactionSerialisationError> {
        let psbt = Decodable::consensus_decode(raw_psbt)?;
        let psbt = utils::psbt_common_sanity_checks(psbt)?;

        if psbt.inputs.is_empty() {
            return Err(PsbtValidationError::InvalidInputCount(0).into());
        }

        let mut max_sat_weight = 0;
        for input in psbt.inputs.iter() {
            // We must only have Unvault inputs
            let txo = input
                .witness_utxo
                .as_ref()
                .expect("Checked by the common parsing checks");
            if !txo.script_pubkey.is_v0_p2wsh() {
                return Err(PsbtValidationError::InvalidInputField(input.clone()).into());
            }

            if input.final_script_witness.is_some() {
                continue;
            }

            if input.sighash_type != Some(SigHashType::All) {
                return Err(PsbtValidationError::InvalidSighashType(input.clone()).into());
            }

            // The Unvault input must contain a valid witness script
            if let Some(ref ws) = input.witness_script {
                if ws.to_v0_p2wsh() != txo.script_pubkey {
                    return Err(PsbtValidationError::InvalidInWitnessScript(input.clone()).into());
                }
            } else {
                return Err(PsbtValidationError::MissingInWitnessScript(input.clone()).into());
            }

            // And since it has a witscript it must have derivation paths for it set
            if input.bip32_derivation.is_empty() {
                return Err(PsbtValidationError::InvalidInputField(input.clone()).into());
            }

            max_sat_weight += miniscript::descriptor::Wsh::new(
                miniscript::Miniscript::parse(
                    input
                        .witness_script
                        .as_ref()
                        .ok_or_else(|| PsbtValidationError::InvalidInputField(input.clone()))?,
                )
                .map_err(|_| PsbtValidationError::InvalidInputField(input.clone()))?,
            )
            .map_err(|_| PsbtValidationError::InvalidInputField(input.clone()))?
            .max_satisfaction_weight()
            .map_err(|_| PsbtValidationError::InvalidInputField(input.clone()))?;
        }

        let mut derivation_count: usize = 0;
        for o in psbt.outputs.iter() {
            if !o.bip32_derivation.is_empty() {
                derivation_count += 1;
                if derivation_count > 2 {
                    return Err(PsbtValidationError::InvalidCountOuputWithDerivations(
                        derivation_count,
                    )
                    .into());
                }
            }
        }
        if derivation_count < 1 {
            return Err(
                PsbtValidationError::InvalidCountOuputWithDerivations(derivation_count).into(),
            );
        }

        // Make sure the transaction cannot get out of standardness bounds once finalized
        let spend_tx = SpendTransaction(psbt);
        let witstrip_weight = spend_tx.psbt().global.unsigned_tx.get_weight();
        let total_weight = witstrip_weight
            .checked_add(max_sat_weight)
            .expect("Weight computation bug");
        if total_weight > MAX_STANDARD_TX_WEIGHT as usize {
            return Err(PsbtValidationError::TransactionTooLarge.into());
        }

        Ok(spend_tx)
    }
}

impl CpfpableTransaction for SpendTransaction {
    fn max_weight(&self) -> u64 {
        let psbt = self.psbt();
        let tx = &psbt.global.unsigned_tx;

        let mut weight: u64 = tx.get_weight().try_into().expect("Can't be >u64::MAX");
        for txin in psbt.inputs.iter() {
            let txin_weight: u64 = if self.is_finalized() {
                txin.final_script_witness
                    .as_ref()
                    .expect("Always set if final")
                    .iter()
                    .map(|e| e.len())
                    .sum::<usize>()
                    .try_into()
                    .expect("Bug: witness size >u64::MAX")
            } else {
                // FIXME: this panic can probably be triggered...
                miniscript::descriptor::Wsh::new(
                    miniscript::Miniscript::parse(
                        txin.witness_script
                            .as_ref()
                            .expect("Unvault txins always have a witness Script"),
                    )
                    .expect("UnvaultTxIn witness_script is created from a Miniscript"),
                )
                .expect("")
                .max_satisfaction_weight()
                .expect("It's a sane Script, derived from a Miniscript")
                .try_into()
                .expect("Can't be >u64::MAX")
            };
            weight = weight
                .checked_add(txin_weight)
                .expect("Weight computation bug: overflow computing spent coins value");
        }
        assert!(weight > 0, "We never create an empty tx");

        weight
    }
}
