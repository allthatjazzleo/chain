use std::{
    convert::TryInto,
    io::{Read, Write},
    net::TcpStream,
    sync::{Arc, Mutex},
};

use parity_scale_codec::{Decode, Encode};
use secp256k1::key::PublicKey;
use zeroize::Zeroize;

use chain_core::{
    common::H256,
    state::account::WithdrawUnbondedTx,
    tx::{
        data::{access::TxAccessPolicy, attribute::TxAttributes, Tx},
        TxWithOutputs,
    },
};
use enclave_protocol::{DecryptionRequest, DecryptionResponse, EnclaveRequest, EnclaveResponse};
use enclave_utils::SealedData;

pub fn get_random_challenge() -> H256 {
    rand::random()
}

pub fn verify_decryption_request(decryption_request: &DecryptionRequest, challenge: H256) -> bool {
    let secp = secp256k1::SECP256K1;
    decryption_request.verify(&secp, challenge).is_ok()
}

pub fn handle_decryption_request(
    decryption_request: &DecryptionRequest,
    chain_data_stream: Arc<Mutex<TcpStream>>,
) -> Result<DecryptionResponse, String> {
    // Prepare enclave request
    let enclave_request = EnclaveRequest::GetSealedTxData {
        txids: decryption_request.body.txs.clone(),
    }
    .encode();

    let mut chain_data_stream = chain_data_stream.lock().unwrap();

    // Send request to chain-abci
    chain_data_stream
        .write_all(&enclave_request)
        .map_err(|err| format!("Error while writing request to chain-abci: {}", err))?;

    // Read reponse length from chain-abci (little endian u32 bytes)
    let mut response_len = [0u8; 4];
    chain_data_stream.read(&mut response_len).map_err(|err| {
        format!(
            "Error while reading reponse length from chain-abci: {}",
            err
        )
    })?;

    let response_len: usize = u32::from_le_bytes(response_len)
        .try_into()
        .map_err(|_| "Response length exceeds `usize` bounds".to_owned())?;
    if response_len == 0 {
        return Err("Unexpected response from chain-abci".to_owned());
    }
    // Read result from chain-abci
    let mut result_buf = vec![0u8; response_len];
    chain_data_stream
        .read(&mut result_buf)
        .map_err(|err| format!("Error while reading response from chain-abci: {}", err))?;

    match EnclaveResponse::decode(&mut result_buf.as_ref()) {
        Ok(EnclaveResponse::GetSealedTxData(Some(sealed_logs))) => {
            let txids = decryption_request.body.txs.clone();
            let view_key = decryption_request.body.view_key;
            let mut return_result = Vec::with_capacity(sealed_logs.len());

            for (txid, sealed_log) in txids.into_iter().zip(sealed_logs.into_iter()) {
                let sealed_data = match SealedData::try_copy_from(&sealed_log) {
                    Some(sealed_data) => sealed_data,
                    None => {
                        return Err(
                            "Unable to parse sealed data returned from chain-abci".to_owned()
                        )
                    }
                };

                if sealed_data.aes_data.additional_txt != txid {
                    return Err("Transaction ID does not match in sealed data".to_owned());
                }

                let mut unsealed_data = sealed_data
                    .unseal()
                    .map_err(|e| format!("Error while unsealing sealed data: {:?}", e))?;
                let otx = TxWithOutputs::decode(&mut unsealed_data.as_slice());
                let push: bool;

                match &otx {
                    Ok(TxWithOutputs::Transfer(Tx {
                        attributes: TxAttributes { allowed_view, .. },
                        ..
                    })) => {
                        push = is_allowed_view(&allowed_view, &view_key);
                    }
                    Ok(TxWithOutputs::StakeWithdraw(WithdrawUnbondedTx {
                        attributes: TxAttributes { allowed_view, .. },
                        ..
                    })) => {
                        push = is_allowed_view(&allowed_view, &view_key);
                    }
                    _ => {
                        return Err("Invalid transaction type".to_owned());
                    }
                }

                if push {
                    return_result.push(otx.unwrap());
                }

                unsealed_data.zeroize();
            }

            let decryption_response = DecryptionResponse { txs: return_result };
            Ok(decryption_response)
        }
        Ok(_) => Err("Unexpected response from chain-abci".to_owned()),
        Err(err) => Err(format!(
            "Error while decoding response from chain-abci: {}",
            err
        )),
    }
}

#[inline]
fn is_allowed_view(allowed_views: &[TxAccessPolicy], view_key: &PublicKey) -> bool {
    // TODO: policy != alldata + const eq?
    allowed_views.iter().any(|x| x.view_key == *view_key)
}
