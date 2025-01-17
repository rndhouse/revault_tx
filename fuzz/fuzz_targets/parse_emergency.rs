#![no_main]
use libfuzzer_sys::fuzz_target;

use revault_tx::{
    miniscript::bitcoin::{
        secp256k1::{Signature, SECP256K1},
        SigHashType,
    },
    transactions::{EmergencyTransaction, RevaultTransaction},
};

use std::str::FromStr;

fuzz_target!(|data: &[u8]| {
    if let Ok(mut tx) = EmergencyTransaction::from_psbt_serialized(data) {
        // We can serialize it back
        tx.as_psbt_serialized();

        // We can network serialize it (without witness data)
        tx.clone().into_bitcoin_serialized();

        let dummykey = secp256k1::PublicKey::from_str(
            "02ca06be8e497d578314c77ca735aa5fcca76d8a5b04019b7a80ff0baaf4a6cf46",
        )
        .unwrap();
        let dummy_sig = Signature::from_str("3045022100e6ffa6cc76339944fa428bcd058a27d0e660d0554a418a79620d7e14cda4cbde022045ba1bcec9fbbdcb4b70328dc7efae7ee59ff496aa8139c81a10b898911b8b52").unwrap();

        let deposit_in_index = tx
            .psbt()
            .inputs
            .iter()
            .position(|i| i.witness_utxo.as_ref().unwrap().script_pubkey.is_v0_p2wsh())
            .unwrap();

        if !tx.is_finalized() {
            // Derivation paths must always be set
            assert!(!tx.psbt().inputs[deposit_in_index]
                .bip32_derivation
                .is_empty());

            // We can compute the sighash for the unvault input
            tx.signature_hash(deposit_in_index, SigHashType::AllPlusAnyoneCanPay)
                .expect("Must be in bound as it was parsed!");

            // We can add a signature, it just is invalid
            assert!(tx
                .add_emer_sig(dummykey, dummy_sig, &SECP256K1)
                .unwrap_err()
                .to_string()
                .contains("Invalid signature"));
        } else {
            // But not if it's final
            assert!(tx
                .signature_hash(deposit_in_index, SigHashType::AllPlusAnyoneCanPay)
                .unwrap_err()
                .to_string()
                .contains("Missing witness_script"));
            assert!(tx
                .add_emer_sig(dummykey, dummy_sig, &SECP256K1)
                .unwrap_err()
                .to_string()
                .contains("already finalized"));
        }

        if tx.tx().input.len() > 1 {
            let fb_in_index = tx
                .psbt()
                .inputs
                .iter()
                .position(|i| {
                    i.witness_utxo
                        .as_ref()
                        .unwrap()
                        .script_pubkey
                        .is_v0_p2wpkh()
                })
                .unwrap();

            tx.add_signature(fb_in_index, dummykey, dummy_sig, &SECP256K1)
                .expect_err("Invalid signature"); // Invalid sighash
            if !tx.is_finalized() {
                assert!(tx
                    .add_signature(fb_in_index, dummykey, dummy_sig, &SECP256K1)
                    .unwrap_err()
                    .to_string()
                    .contains("Invalid signature"));
            } else {
                assert!(tx
                    .add_signature(fb_in_index, dummykey, dummy_sig, &SECP256K1)
                    .unwrap_err()
                    .to_string()
                    .contains("already finalized"));
            }
        } else {
            assert!(tx
                .add_signature(1, dummykey, dummy_sig, &SECP256K1)
                .unwrap_err()
                .to_string()
                .contains("out of bounds"));
        }

        // And verify the input without crashing (will likely fail though)
        tx.verify_inputs().unwrap_or_else(|_| ());

        // Same for the finalization
        tx.finalize(&SECP256K1).unwrap_or_else(|_| ());
    }
});
