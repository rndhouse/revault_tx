use crate::{
    error::*,
    transactions::{utils, CPFP_MIN_CHANGE},
    txins::*,
    txouts::*,
};

use miniscript::bitcoin::{
    consensus::encode,
    util::psbt::{Global as PsbtGlobal, Input as PsbtIn, PartiallySignedTransaction as Psbt},
    Amount, Script, SigHashType, Transaction, TxIn,
};

use std::{convert::TryInto, fmt};

// If single-input single-output we need this many dummy vbytes to keep our transaction above the
// minimum standard size.
const OP_RETURN_DUMMY_DATA: [u8; 22] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

// Given the current CPFP transaction template, return the OP_RETURN script to use.
//
// The script to use depends on the transaction because of the minimum standard transaction size of
// 82 vbytes (https://github.com/bitcoin/bitcoin/blob/master/src/policy/policy.h). If we'd end up
// with a 1-in 1-out transaction we need to pad the OP_RETURN script in the output with dummy data
// in order to meet the minimum size. Otherwise we can use a minimal OP_RETURN.
fn op_return_script(cpfp_psbt: &Psbt) -> Script {
    if cpfp_psbt.global.unsigned_tx.input.len() > 1 {
        Script::new_op_return(&[])
    } else {
        Script::new_op_return(&OP_RETURN_DUMMY_DATA)
    }
}

/// The transaction spending the Unvault and Spend transactions' CPFP output in order
/// to bump their fees.
#[derive(Clone, PartialEq)]
pub struct CpfpTransaction(Psbt);

// A custom implementation of Debug to make it more compact in logs
impl fmt::Debug for CpfpTransaction {
    fn fmt(&self, f: &mut fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", base64::encode(encode::serialize(&self.0)))
    }
}

impl fmt::Display for CpfpTransaction {
    fn fmt(&self, f: &mut fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", &self)
    }
}

impl CpfpTransaction {
    /// Create a CPFP tx to bump a set of transactions by a specified feerate.
    /// The current implementation will return a CPFP tx that either pays to a 0-value OP_RETURN
    /// output or to a change output paying to the same script. It will select UTxOs to consume
    /// using a largest first CS and will error if not enough UTxOs are provided to cover the expected feerate.
    /// NOTE: we assume all available UTxOs to be CPFP txouts from other Unvault transactions.
    // FIXME: Avoid largest first CS and be smarter! Instead, go with:
    // - calculate the fees we need to cover before being ok
    // - if the biggest coin is less than the fees, take the biggest coin
    // - otherwise, take the smallest coin big enough to cover fees
    pub fn from_txins(
        to_be_cpfped: Vec<CpfpTxIn>,
        tbc_weight: u64,
        tbc_fees: Amount,
        added_feerate: u64,
        mut available_utxos: Vec<CpfpTxIn>,
    ) -> Result<CpfpTransaction, TransactionCreationError> {
        assert!(!to_be_cpfped.is_empty());
        // This will sort the vector in ascending order.
        // Since we're going to pop() from it, we're using a largest first CS.
        available_utxos.sort_unstable_by_key(|l| l.txout().txout().value);

        let mut txins = vec![];
        let mut psbtins = vec![];
        let mut dummy_change = None;
        let mut inputs_sum = Amount::from_sat(0);
        let mut total_satisfation_weight = 0;

        for cpfp_txin in to_be_cpfped {
            dummy_change = Some(cpfp_txin.txout().txout().clone());
            inputs_sum += Amount::from_sat(cpfp_txin.txout().txout().value);
            // I can't collapse this in one call (total_satisfation_weight += ...)
            // as I have a "cannot infer type"
            let w: u64 = cpfp_txin
                .txout()
                .max_sat_weight()
                .try_into()
                .expect("Weight doesn't fit in u64?");
            total_satisfation_weight += w;
            txins.push(TxIn {
                previous_output: cpfp_txin.outpoint(),
                sequence: RBF_SEQUENCE,
                script_sig: Script::new(),
                witness: vec![],
            });
            psbtins.push(PsbtIn {
                witness_script: Some(cpfp_txin.txout().witness_script().clone()),
                bip32_derivation: cpfp_txin.txout().bip32_derivation().clone(),
                sighash_type: Some(SigHashType::All),
                witness_utxo: Some(cpfp_txin.into_txout().into_txout()),
                ..PsbtIn::default()
            });
        }

        let dummy_change = dummy_change.expect("Must be initialized in the loop");

        let transaction = Transaction {
            version: 2,
            lock_time: 0,
            input: txins,
            output: vec![dummy_change],
        };

        let mut psbt = Psbt {
            global: PsbtGlobal::from_unsigned_tx(transaction).expect("unsigned"),
            inputs: psbtins,
            outputs: vec![Default::default()],
        };

        // We discard the CPFP descriptors in to_be_cpfped as we don't need them anymore
        let tbc_feerate = 1_000 * (tbc_fees.as_sat() + tbc_weight) / tbc_weight; // * 1000 for kWU

        let target_feerate = tbc_feerate + added_feerate;
        loop {
            let cpfp_weight: u64 = psbt
                .global
                .unsigned_tx
                .get_weight()
                .try_into()
                .expect("Weight doesn't fit in u64?");
            let package_weight = cpfp_weight + total_satisfation_weight + tbc_weight;
            let fees_needed = Amount::from_sat(
                // /1000 to get sats/WU (rounded down) from sats/kWU
                target_feerate * package_weight / 1000,
            ) - tbc_fees;

            // Here we calculate the fees needed if we used OP_RETURN instead of p2wsh
            // as output
            let mut op_return_tx = psbt.global.unsigned_tx.clone();
            op_return_tx.output[0].script_pubkey = op_return_script(&psbt);
            op_return_tx.output[0].value = 0;
            let opr_tx_weight: u64 = op_return_tx
                .get_weight()
                .try_into()
                .expect("Weight doesn't fit in u64?");
            let opr_package_weight = opr_tx_weight + total_satisfation_weight + tbc_weight;
            let op_return_fees_needed = Amount::from_sat(
                // /1000 to get sats/WU (rounded down) from sats/kWU
                target_feerate * opr_package_weight / 1000,
            ) - tbc_fees;

            if inputs_sum > fees_needed || inputs_sum > op_return_fees_needed {
                // Alright, we found it!
                if inputs_sum > fees_needed && (inputs_sum - fees_needed).as_sat() > CPFP_MIN_CHANGE
                {
                    // If it makes sense to have a change, let's have a change :)
                    let change = &mut psbt.global.unsigned_tx.output[0];
                    change.value = (inputs_sum - fees_needed).as_sat();
                } else {
                    // Otherwise, stick with OP_RETURN
                    let opr = op_return_script(&psbt);
                    let change = &mut psbt.global.unsigned_tx.output[0];
                    change.value = 0;
                    change.script_pubkey = opr;
                }
                return Ok(CpfpTransaction(psbt));
            } else {
                // Let's add another input and start again
                match available_utxos.pop() {
                    Some(new_input) => {
                        psbt.global.unsigned_tx.input.push(TxIn {
                            previous_output: new_input.outpoint(),
                            script_sig: Script::new(),
                            sequence: RBF_SEQUENCE,
                            witness: vec![],
                        });

                        let bip32_derivation = new_input.txout().clone().into_bip32_derivation();

                        psbt.inputs.push(PsbtIn {
                            witness_utxo: Some(new_input.txout().txout().clone()),
                            witness_script: Some(new_input.txout().clone().into_witness_script()),
                            sighash_type: Some(SigHashType::All),
                            bip32_derivation,
                            ..Default::default()
                        });
                        inputs_sum += Amount::from_sat(new_input.txout().txout().value);

                        let input_sat_weight: u64 = new_input
                            .txout()
                            .max_sat_weight()
                            .try_into()
                            .expect("Weight doesn't fit in u64?");
                        total_satisfation_weight += input_sat_weight;
                    }
                    None => {
                        return Err(TransactionCreationError::InsufficientFunds);
                    }
                }
            }
        }
    }

    pub fn psbt(&self) -> &Psbt {
        &self.0
    }

    pub fn into_psbt(self) -> Psbt {
        self.0
    }

    pub fn tx(&self) -> &Transaction {
        &self.psbt().global.unsigned_tx
    }

    pub fn fees(&self) -> u64 {
        // We always set a witness_utxo in our PSBT inputs.
        utils::psbt_fees(self.psbt()).expect("Fee computation bug: overflow")
    }
}
