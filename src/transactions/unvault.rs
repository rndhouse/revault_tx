use crate::{
    error::*,
    scripts::*,
    transactions::{
        utils, CpfpableTransaction, RevaultTransaction, DUST_LIMIT, INSANE_FEES,
        MAX_STANDARD_TX_WEIGHT, TX_VERSION, UNVAULT_CPFP_VALUE, UNVAULT_TX_FEERATE,
    },
    txins::*,
    txouts::*,
};

use miniscript::{
    bitcoin::{
        blockdata::constants::max_money,
        consensus::encode::Decodable,
        secp256k1,
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

use std::{collections::BTreeMap, convert::TryInto};

impl_revault_transaction!(
    UnvaultTransaction,
    doc = "The unvaulting transaction, spending a deposit and being eventually spent by a spend transaction (if not revaulted)."
);
impl UnvaultTransaction {
    // Internal DRY routine for creating the inner PSBT
    fn create_psbt(
        deposit_txin: DepositTxIn,
        unvault_txout: UnvaultTxOut,
        cpfp_txout: CpfpTxOut,
        lock_time: u32,
    ) -> Psbt {
        Psbt {
            // 1 Unvault, 1 CPFP
            outputs: vec![
                PsbtOut {
                    bip32_derivation: unvault_txout.bip32_derivation().clone(),
                    ..PsbtOut::default()
                },
                PsbtOut {
                    bip32_derivation: cpfp_txout.bip32_derivation().clone(),
                    ..PsbtOut::default()
                },
            ],
            global: PsbtGlobal {
                unsigned_tx: Transaction {
                    version: TX_VERSION,
                    lock_time,
                    input: vec![deposit_txin.unsigned_txin()],
                    output: vec![unvault_txout.into_txout(), cpfp_txout.into_txout()],
                },
                version: 0,
                xpub: BTreeMap::new(),
                proprietary: BTreeMap::new(),
                unknown: BTreeMap::new(),
            },
            inputs: vec![PsbtIn {
                witness_script: Some(deposit_txin.txout().witness_script().clone()),
                bip32_derivation: deposit_txin.txout().bip32_derivation().clone(),
                sighash_type: Some(SigHashType::All),
                witness_utxo: Some(deposit_txin.into_txout().into_txout()),
                ..PsbtIn::default()
            }],
        }
    }

    /// An unvault transaction always spends one deposit output and contains one CPFP output in
    /// addition to the unvault one.
    /// It's always created using a fixed feerate and the CPFP output value is fixed as well.
    ///
    /// BIP174 Creator and Updater roles.
    pub fn new(
        deposit_input: DepositTxIn,
        unvault_descriptor: &DerivedUnvaultDescriptor,
        cpfp_descriptor: &DerivedCpfpDescriptor,
        lock_time: u32,
    ) -> Result<UnvaultTransaction, TransactionCreationError> {
        // First, create a dummy transaction to get its weight without Witness
        let dummy_unvault_txout = UnvaultTxOut::new(Amount::from_sat(u64::MAX), unvault_descriptor);
        let dummy_cpfp_txout = CpfpTxOut::new(Amount::from_sat(u64::MAX), cpfp_descriptor);
        let dummy_tx = UnvaultTransaction::create_psbt(
            deposit_input.clone(),
            dummy_unvault_txout,
            dummy_cpfp_txout,
            lock_time,
        )
        .global
        .unsigned_tx;

        // The weight of the transaction once signed will be the size of the witness-stripped
        // transaction plus the size of the single input's witness.
        let total_weight = dummy_tx
            .get_weight()
            .checked_add(deposit_input.txout().max_sat_weight())
            .expect("Properly-computed weights cannot overflow");
        let total_weight: u64 = total_weight.try_into().expect("usize in u64");
        let fees = UNVAULT_TX_FEERATE
            .checked_mul(total_weight)
            .expect("Properly-computed weights cannot overflow");
        // Nobody wants to pay 3k€ fees if we had a bug.
        if fees > INSANE_FEES {
            return Err(TransactionCreationError::InsaneFees);
        }

        assert!(
            total_weight <= MAX_STANDARD_TX_WEIGHT as u64,
            "A single input and two outputs"
        );

        // The unvault output value is then equal to the deposit value minus the fees and the CPFP.
        let deposit_value = deposit_input.txout().txout().value;
        if fees + UNVAULT_CPFP_VALUE + DUST_LIMIT > deposit_value {
            return Err(TransactionCreationError::Dust);
        }
        let unvault_value = deposit_value - fees - UNVAULT_CPFP_VALUE; // Arithmetic checked above
        if unvault_value > max_money(Network::Bitcoin) {
            return Err(TransactionCreationError::InsaneAmounts);
        }

        let unvault_txout = UnvaultTxOut::new(Amount::from_sat(unvault_value), unvault_descriptor);
        let cpfp_txout = CpfpTxOut::new(Amount::from_sat(UNVAULT_CPFP_VALUE), cpfp_descriptor);
        Ok(UnvaultTransaction(UnvaultTransaction::create_psbt(
            deposit_input,
            unvault_txout,
            cpfp_txout,
            lock_time,
        )))
    }

    fn unvault_txin(
        &self,
        unvault_descriptor: &DerivedUnvaultDescriptor,
        sequence: u32,
    ) -> UnvaultTxIn {
        let spk = unvault_descriptor.inner().script_pubkey();
        let index = self
            .psbt()
            .global
            .unsigned_tx
            .output
            .iter()
            .position(|txo| txo.script_pubkey == spk)
            .expect("UnvaultTransaction is always created with an Unvault txo");

        // Unwraped above
        let txo = &self.psbt().global.unsigned_tx.output[index];
        let prev_txout = UnvaultTxOut::new(Amount::from_sat(txo.value), unvault_descriptor);
        UnvaultTxIn::new(
            OutPoint {
                txid: self.psbt().global.unsigned_tx.txid(),
                vout: index.try_into().expect("There are two outputs"),
            },
            prev_txout,
            sequence,
        )
    }

    /// Get the Unvault txo to be referenced in a spending transaction
    pub fn spend_unvault_txin(&self, unvault_descriptor: &DerivedUnvaultDescriptor) -> UnvaultTxIn {
        self.unvault_txin(unvault_descriptor, unvault_descriptor.csv_value())
    }

    /// Get the Unvault txo to be referenced in a revocation transaction
    pub fn revault_unvault_txin(
        &self,
        unvault_descriptor: &DerivedUnvaultDescriptor,
    ) -> UnvaultTxIn {
        self.unvault_txin(unvault_descriptor, RBF_SEQUENCE)
    }

    /// Parse an Unvault transaction from a PSBT
    pub fn from_raw_psbt(raw_psbt: &[u8]) -> Result<Self, TransactionSerialisationError> {
        let psbt = Decodable::consensus_decode(raw_psbt)?;
        let psbt = utils::psbt_common_sanity_checks(psbt)?;

        // Unvault + CPFP txos
        let output_count = psbt.global.unsigned_tx.output.len();
        if output_count != 2 {
            return Err(PsbtValidationError::InvalidOutputCount(output_count).into());
        }

        for output in psbt.outputs.iter() {
            if output.bip32_derivation.is_empty() {
                return Err(PsbtValidationError::InvalidOutputField(output.clone()).into());
            }
        }

        let input_count = psbt.global.unsigned_tx.input.len();
        // We for now have 1 unvault == 1 deposit
        if input_count != 1 {
            return Err(PsbtValidationError::InvalidInputCount(input_count).into());
        }
        let input = &psbt.inputs[0];
        if input.final_script_witness.is_none() {
            if input.sighash_type != Some(SigHashType::All) {
                return Err(PsbtValidationError::InvalidSighashType(input.clone()).into());
            }

            if input.bip32_derivation.is_empty() {
                return Err(PsbtValidationError::InvalidInputField(input.clone()).into());
            }

            if let Some(ref ws) = input.witness_script {
                if ws.to_v0_p2wsh()
                    != input
                        .witness_utxo
                        .as_ref()
                        .expect("Check in sanity checks")
                        .script_pubkey
                {
                    return Err(PsbtValidationError::InvalidInWitnessScript(input.clone()).into());
                }
            } else {
                return Err(PsbtValidationError::MissingInWitnessScript(input.clone()).into());
            }
        }

        // NOTE: the Unvault transaction cannot get larger than MAX_STANDARD_TX_WEIGHT

        Ok(UnvaultTransaction(psbt))
    }

    /// Add a signature for the (single) input spending the Deposit transaction
    pub fn add_sig<C: secp256k1::Verification>(
        &mut self,
        pubkey: secp256k1::PublicKey,
        signature: secp256k1::Signature,
        secp: &secp256k1::Secp256k1<C>,
    ) -> Result<Option<Vec<u8>>, InputSatisfactionError> {
        // We are only ever created with a single input
        let input_index = 0;
        RevaultTransaction::add_signature(self, input_index, pubkey, signature, secp)
    }
}

impl CpfpableTransaction for UnvaultTransaction {
    fn max_weight(&self) -> u64 {
        let psbt = self.psbt();
        let tx = &psbt.global.unsigned_tx;

        // We are only ever created with exactly one input.
        let txin = &psbt.inputs[0];
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

        let weight: u64 = tx.get_weight().try_into().expect("Can't be >u64::MAX");
        let weight = weight + txin_weight;
        assert!(weight > 0, "We never create an empty tx");
        weight
    }
}
