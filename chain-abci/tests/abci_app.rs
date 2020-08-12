use abci::*;
use bit_vec::BitVec;
use chain_abci::app::*;
use chain_abci::enclave_bridge::mock::MockClient;
use chain_abci::staking::StakingTable;
use chain_core::common::{MerkleTree, Proof, H256, HASH_SIZE_256};
use chain_core::compute_app_hash;
use chain_core::init::address::RedeemAddress;
use chain_core::init::coin::Coin;
use chain_core::init::config::InitConfig;
use chain_core::init::config::InitNetworkParameters;
use chain_core::init::config::NetworkParameters;
use chain_core::init::config::{
    JailingParameters, RewardsParameters, SlashRatio, SlashingParameters,
};
use chain_core::state::account::{
    DepositBondTx, NodeState, StakedState, StakedStateAddress, StakedStateDestination,
    StakedStateOpAttributes, StakedStateOpWitness, UnbondTx, WithdrawUnbondedTx,
};
use chain_core::state::tendermint::{
    BlockHeight, TendermintValidatorAddress, TendermintValidatorPubKey, TendermintVotePower,
};
use chain_core::state::validator::NodeJoinRequestTx;
use chain_core::state::{ChainState, RewardsPoolState};
use chain_core::tx::fee::{LinearFee, Milli};
use chain_core::tx::witness::tree::RawXOnlyPubkey;
use chain_core::tx::witness::EcdsaSignature;
use chain_core::tx::{
    data::{
        access::{TxAccess, TxAccessPolicy},
        address::ExtendedAddr,
        attribute::TxAttributes,
        input::{TxoPointer, TxoSize},
        output::TxOut,
        Tx, TxId,
    },
    witness::{TxInWitness, TxWitness},
    PlainTxAux, TransactionId, TxAux, TxEnclaveAux, TxPublicAux,
};
use chain_storage::buffer::Get;
use chain_storage::jellyfish::SparseMerkleProof;
use chain_storage::{
    LookupItem, Storage, CHAIN_ID_KEY, COL_EXTRA, COL_NODE_INFO, GENESIS_APP_HASH_KEY,
    LAST_STATE_KEY, NUM_COLUMNS,
};
use chain_tx_filter::BlockFilter;
use hex::decode;
use kvdb::KeyValueDB;
use kvdb_memorydb::create;
use mock_utils::encrypt;
use parity_scale_codec::{Decode, Encode};
use secp256k1::schnorrsig::schnorr_sign;
use secp256k1::{key::PublicKey, key::SecretKey, key::XOnlyPublicKey, Message, Secp256k1, Signing};
use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::convert::TryInto;
use std::str::FromStr;
use std::sync::Arc;
use test_common::chain_env::{
    mock_confidential_init, mock_council_node_join, ChainEnv, DEFAULT_GENESIS_TIME,
};

const TEST_CHAIN_ID: &str = "test-00";
const EXAMPLE_HASH: &str = "F5E8DFBF717082D6E9508E1A5A5C9B8EAC04A39F69C40262CB733C920DA10962";

pub fn get_enclave_bridge_mock() -> MockClient {
    MockClient::new(0)
}

pub fn get_ecdsa_witness<C: Signing>(
    secp: &Secp256k1<C>,
    txid: &TxId,
    secret_key: &SecretKey,
) -> EcdsaSignature {
    let message = Message::from_slice(&txid[..]).expect("32 bytes");
    secp.sign_recoverable(&message, &secret_key)
}

fn create_db() -> Arc<dyn KeyValueDB> {
    Arc::new(create(NUM_COLUMNS))
}

fn create_db_with_state_history() -> Arc<dyn KeyValueDB> {
    let db = create_db();
    let decoded_gah = decode(EXAMPLE_HASH).unwrap();
    let mut genesis_app_hash = [0u8; HASH_SIZE_256];
    genesis_app_hash.copy_from_slice(&decoded_gah[..]);
    let mut inittx = db.transaction();
    inittx.put(COL_NODE_INFO, GENESIS_APP_HASH_KEY, &genesis_app_hash);
    inittx.put(
        COL_NODE_INFO,
        LAST_STATE_KEY,
        &get_dummy_app_state(genesis_app_hash).encode(),
    );
    inittx.put(COL_EXTRA, CHAIN_ID_KEY, TEST_CHAIN_ID.as_bytes());

    db.write(inittx).unwrap();
    db
}

#[test]
fn proper_hash_and_chainid_should_be_stored() {
    let db = create_db();
    let example_hash = "F5E8DFBF717082D6E9508E1A5A5C9B8EAC04A39F69C40262CB733C920DA10962";
    let app = ChainNodeApp::new_with_storage(
        get_enclave_bridge_mock(),
        example_hash,
        TEST_CHAIN_ID,
        Storage::new_db(db.clone()),
        None,
    );
    let decoded_gah = decode(example_hash).unwrap();
    let stored_genesis = app.storage.get_genesis_app_hash();
    assert_eq!(decoded_gah, stored_genesis);
    let chain_id = app.storage.get_stored_chain_id();
    assert_eq!(chain_id, TEST_CHAIN_ID.as_bytes());
}

#[test]
fn proper_last_state_should_be_restored() {
    let db = create_db_with_state_history();
    let storage = Storage::new_db(db.clone());
    assert!(storage.get_last_app_state().is_some());
    let app = ChainNodeApp::new_with_storage(
        get_enclave_bridge_mock(),
        EXAMPLE_HASH,
        TEST_CHAIN_ID,
        storage,
        None,
    );
    let decoded_gah = decode(EXAMPLE_HASH).unwrap();
    let stored_genesis = app.storage.get_genesis_app_hash();
    assert_eq!(decoded_gah, stored_genesis);
    let chain_id = app.storage.get_stored_chain_id();
    assert_eq!(chain_id, TEST_CHAIN_ID.as_bytes());
}

#[test]
#[should_panic]
fn too_long_hash_should_panic() {
    let db = create_db();
    let example_hash = "F5E8DFBF717082D6E9508E1A5A5C9B8EAC04A39F69C40262CB733C920DA10962F5E8DFBF717082D6E9508E1A5A5C9B8EAC04A39F69C40262CB733C920DA10962";
    let _app = ChainNodeApp::new_with_storage(
        get_enclave_bridge_mock(),
        example_hash,
        TEST_CHAIN_ID,
        Storage::new_db(db.clone()),
        None,
    );
}

#[test]
#[should_panic]
fn chain_id_without_hex_digits_should_panic() {
    let db = create_db();
    let example_hash = "F5E8DFBF717082D6E9508E1A5A5C9B8EAC04A39F69C40262CB733C920DA10962";
    let _app = ChainNodeApp::new_with_storage(
        get_enclave_bridge_mock(),
        example_hash,
        "test",
        Storage::new_db(db.clone()),
        None,
    );
}

#[test]
#[should_panic]
fn nonhex_hash_should_panic() {
    let db = create_db();
    let example_hash = "EOWNEOIWFNOPXZ./32";
    let _app = ChainNodeApp::new_with_storage(
        get_enclave_bridge_mock(),
        example_hash,
        TEST_CHAIN_ID,
        Storage::new_db(db.clone()),
        None,
    );
}

fn get_dummy_network_params() -> NetworkParameters {
    NetworkParameters::Genesis(InitNetworkParameters {
        initial_fee_policy: LinearFee::new(
            Milli::try_new(1, 1).unwrap(),
            Milli::try_new(1, 1).unwrap(),
        ),
        required_council_node_stake: Coin::unit(),
        jailing_config: JailingParameters {
            block_signing_window: 100,
            missed_block_threshold: 50,
        },
        slashing_config: SlashingParameters {
            liveness_slash_percent: SlashRatio::from_str("0.1").unwrap(),
            byzantine_slash_percent: SlashRatio::from_str("0.2").unwrap(),
        },
        rewards_config: RewardsParameters {
            monetary_expansion_cap: Coin::zero(),
            reward_period_seconds: 24 * 60 * 60,
            monetary_expansion_r0: "0.5".parse().unwrap(),
            monetary_expansion_tau: 166_666_600,
            monetary_expansion_decay: 999_860,
        },
        max_validators: 2,
    })
}

fn get_dummy_app_state(app_hash: H256) -> ChainNodeState {
    let params = get_dummy_network_params();
    ChainNodeState {
        last_block_height: BlockHeight::genesis(),
        last_apphash: app_hash,
        block_time: 0,
        block_height: BlockHeight::genesis(),
        genesis_time: 0,
        max_evidence_age: 172_800,
        staking_table: StakingTable::default(),
        staking_version: 0,
        utxo_coins: Coin::zero(),
        enclave_isv_svn: 0,
        top_level: ChainState {
            account_root: [0u8; 32],
            rewards_pool: RewardsPoolState::new(0, params.get_rewards_monetary_expansion_tau()),
            network_params: params,
        },
    }
}

#[test]
#[should_panic]
fn previously_stored_hash_should_match() {
    let db = create_db_with_state_history();
    let error_hash = "F5E8DFBF717082D6E9508E1A5A5C9B8EAC04A39F69C40262CB733C920DA10963";
    let _app = ChainNodeApp::new_with_storage(
        get_enclave_bridge_mock(),
        error_hash,
        TEST_CHAIN_ID,
        Storage::new_db(db),
        None,
    );
}

fn init_chain_for(address: RedeemAddress) -> ChainNodeApp<MockClient> {
    let db = create_db();
    let total = (Coin::max() - Coin::unit()).unwrap();
    let validator_addr = "0x0e7c045110b8dbf29765047380898919c5cb56f4"
        .parse::<RedeemAddress>()
        .unwrap();

    let distribution = [
        (
            address,
            (StakedStateDestination::UnbondedFromGenesis, total),
        ),
        (
            validator_addr,
            (StakedStateDestination::Bonded, Coin::unit()),
        ),
    ]
    .iter()
    .cloned()
    .collect();
    let NetworkParameters::Genesis(params) = get_dummy_network_params();
    let mut nodes = BTreeMap::new();
    let pub_key =
        TendermintValidatorPubKey::from_base64(b"MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDA=")
            .unwrap();
    let node_pubkey = (
        "test".to_owned(),
        None,
        pub_key.clone(),
        mock_confidential_init(),
    );
    let validator = ValidatorUpdate {
        pub_key: Some(PubKey {
            field_type: "ed25519".to_owned(),
            data: pub_key.as_bytes().to_vec(),
            ..Default::default()
        })
        .into(),
        power: TendermintVotePower::from(Coin::unit()).into(),
        ..Default::default()
    };
    nodes.insert(validator_addr, node_pubkey);
    let c = InitConfig::new(distribution, params, nodes);
    let t = ::protobuf::well_known_types::Timestamp {
        seconds: DEFAULT_GENESIS_TIME as i64,
        ..Default::default()
    };
    let result = c.validate_config_get_genesis(t.get_seconds().try_into().unwrap());
    if let Ok(genesis_state) = result {
        let tx_tree = MerkleTree::empty();
        let mut storage = Storage::new_db(db.clone());
        let new_account_root = storage.put_stakings(0, &genesis_state.accounts);
        let genesis_app_hash = compute_app_hash(
            &tx_tree,
            &new_account_root,
            &genesis_state.rewards_pool,
            &get_dummy_network_params(),
        );

        let example_hash = hex::encode_upper(genesis_app_hash);
        let mut app = ChainNodeApp::new_with_storage(
            get_enclave_bridge_mock(),
            &example_hash,
            TEST_CHAIN_ID,
            storage,
            None,
        );
        let mut req = RequestInitChain::default();
        req.set_time(t);
        req.set_app_state_bytes(serde_json::to_vec(&c).unwrap());
        req.set_chain_id(String::from(TEST_CHAIN_ID));
        req.set_validators(vec![validator].into());
        req.set_consensus_params(ConsensusParams {
            evidence: Some(EvidenceParams {
                max_age_duration: Some(::protobuf::well_known_types::Duration {
                    seconds: 172_800,
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        });
        app.init_chain(&req);
        app
    } else {
        panic!("distribution validation error: {}", result.err().unwrap());
    }
}

#[test]
fn init_chain_should_create_db_items() {
    let address = "0xfe7c045110b8dbf29765047380898919c5cb56f9"
        .parse()
        .unwrap();
    let app = init_chain_for(address);
    let genesis_app_hash = app.genesis_app_hash;
    let state =
        ChainNodeState::decode(&mut app.storage.get_last_app_state().unwrap().as_slice()).unwrap();

    assert_eq!(genesis_app_hash, state.last_apphash);
    app.staking_getter_committed()
        .get(&address.into())
        .expect("account not exists");
}

#[test]
#[should_panic]
fn init_chain_panics_with_different_app_hash() {
    let db = create_db();
    let expansion_cap = Coin::zero();
    let distribution = [(
        "0x0e7c045110b8dbf29765047380898919c5cb56f4"
            .parse()
            .unwrap(),
        (StakedStateDestination::Bonded, Coin::max()),
    )]
    .iter()
    .cloned()
    .collect();
    let params = InitNetworkParameters {
        initial_fee_policy: LinearFee::new(
            Milli::try_new(1, 1).unwrap(),
            Milli::try_new(1, 1).unwrap(),
        ),
        required_council_node_stake: Coin::unit(),
        jailing_config: JailingParameters {
            block_signing_window: 100,
            missed_block_threshold: 50,
        },
        slashing_config: SlashingParameters {
            liveness_slash_percent: SlashRatio::from_str("0.1").unwrap(),
            byzantine_slash_percent: SlashRatio::from_str("0.2").unwrap(),
        },
        rewards_config: RewardsParameters {
            monetary_expansion_cap: expansion_cap,
            reward_period_seconds: 24 * 60 * 60,
            monetary_expansion_r0: "0.5".parse().unwrap(),
            monetary_expansion_tau: 166_666_600,
            monetary_expansion_decay: 999_860,
        },
        max_validators: 1,
    };
    let c = InitConfig::new(distribution, params, BTreeMap::new());

    let example_hash = "F5E8DFBF717082D6E9508E1A5A5C9B8EAC04A39F69C40262CB733C920DA10963";
    let mut app = ChainNodeApp::new_with_storage(
        get_enclave_bridge_mock(),
        &example_hash,
        TEST_CHAIN_ID,
        Storage::new_db(db.clone()),
        None,
    );
    let mut req = RequestInitChain::default();
    req.set_app_state_bytes(serde_json::to_vec(&c).unwrap());
    req.set_time(::protobuf::well_known_types::Timestamp::new());
    req.set_chain_id(String::from(TEST_CHAIN_ID));
    app.init_chain(&req);
}

#[test]
#[should_panic]
fn init_chain_panics_with_empty_app_bytes() {
    let db = create_db();
    let example_hash = "F5E8DFBF717082D6E9508E1A5A5C9B8EAC04A39F69C40262CB733C920DA10963";
    let mut app = ChainNodeApp::new_with_storage(
        get_enclave_bridge_mock(),
        &example_hash,
        TEST_CHAIN_ID,
        Storage::new_db(db.clone()),
        None,
    );
    let req = RequestInitChain::default();
    app.init_chain(&req);
}

#[test]
fn check_tx_should_reject_empty_tx() {
    let mut app = init_chain_for(
        "0xfe7c045110b8dbf29765047380898919c5cb56f9"
            .parse()
            .unwrap(),
    );
    let creq = RequestCheckTx::default();
    let cresp = app.check_tx(&creq);
    assert_ne!(0, cresp.code);
}

#[test]
fn check_tx_should_reject_invalid_tx() {
    let mut app = init_chain_for(
        "0xfe7c045110b8dbf29765047380898919c5cb56f9"
            .parse()
            .unwrap(),
    );
    let mut creq = RequestCheckTx::default();
    let tx = PlainTxAux::new(Tx::new(), TxWitness::new());
    creq.set_tx(tx.encode());
    let cresp = app.check_tx(&creq);
    assert_ne!(0, cresp.code);
}

fn prepare_app_valid_tx() -> (ChainNodeApp<MockClient>, TxAux, WithdrawUnbondedTx) {
    let secp = secp256k1::SECP256K1;
    let secret_key = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);
    let addr = RedeemAddress::from(&public_key);
    let app = init_chain_for(addr);
    let tx = WithdrawUnbondedTx::new(
        0,
        vec![
            TxOut::new_with_timelock(
                ExtendedAddr::OrTree([0; 32]),
                Coin::one(),
                DEFAULT_GENESIS_TIME,
            ),
            TxOut::new_with_timelock(
                ExtendedAddr::OrTree([1; 32]),
                Coin::unit(),
                DEFAULT_GENESIS_TIME,
            ),
            // leftover -- in previous tests, it was all paid as a fee
            TxOut::new_with_timelock(
                ExtendedAddr::OrTree([2; 32]),
                Coin::new(9999999999899999651).unwrap(),
                DEFAULT_GENESIS_TIME,
            ),
        ],
        TxAttributes::new_with_access(0, vec![TxAccessPolicy::new(public_key, TxAccess::AllData)]),
    );

    let witness = StakedStateOpWitness::new(get_ecdsa_witness(&secp, &tx.id(), &secret_key));
    let txaux = TxAux::EnclaveTx(TxEnclaveAux::WithdrawUnbondedStakeTx {
        no_of_outputs: tx.outputs.len() as TxoSize,
        witness: witness.clone(),
        payload: encrypt(&PlainTxAux::WithdrawUnbondedStakeTx(tx.clone()), tx.id()),
    });
    (app, txaux, tx)
}

#[test]
fn check_tx_should_accept_valid_tx() {
    let (mut app, txaux, _) = prepare_app_valid_tx();
    let mut creq = RequestCheckTx::default();
    creq.set_tx(txaux.encode());
    let cresp = app.check_tx(&creq);
    assert_eq!(0, cresp.code, "{}", cresp.log);
}

#[test]
#[should_panic]
fn two_beginblocks_should_panic() {
    let mut app = init_chain_for(
        "0x0e7c045110b8dbf29765047380898919c5cb56f4"
            .parse()
            .unwrap(),
    );
    let bbreq = RequestBeginBlock::default();
    app.begin_block(&bbreq);
    app.begin_block(&bbreq);
}

fn get_block_proposer(app: &ChainNodeApp<MockClient>) -> TendermintValidatorAddress {
    let StakedStateAddress::BasicRedeem(staking_address) = app
        .last_state
        .as_ref()
        .unwrap()
        .staking_table
        .get_chosen_validators()
        .iter()
        .next()
        .unwrap()
        .0;

    match get_account(staking_address, app)
        .unwrap()
        .node_meta
        .unwrap()
    {
        NodeState::CouncilNode(v) => v.validator_address(),
        _ => unreachable!(),
    }
}

fn begin_block(app: &mut ChainNodeApp<MockClient>) {
    let mut bbreq = RequestBeginBlock::default();
    let mut header = Header::default();
    header.set_time(::protobuf::well_known_types::Timestamp {
        seconds: DEFAULT_GENESIS_TIME as i64,
        ..Default::default()
    });
    header.set_proposer_address(Into::<[u8; 20]>::into(&get_block_proposer(app)).to_vec());
    bbreq.set_header(header);
    app.begin_block(&bbreq);
}

#[test]
fn deliver_tx_should_reject_empty_tx() {
    let mut app = init_chain_for(
        "0xfe7c045110b8dbf29765047380898919c5cb56f9"
            .parse()
            .unwrap(),
    );
    assert_eq!(0, app.delivered_txs.len());
    begin_block(&mut app);
    let creq = RequestDeliverTx::default();
    let cresp = app.deliver_tx(&creq);
    assert_ne!(0, cresp.code);
    assert_eq!(0, app.delivered_txs.len());
    assert_eq!(0, cresp.events.len());
}

#[test]
fn deliver_tx_should_reject_invalid_tx() {
    let mut app = init_chain_for(
        "0xfe7c045110b8dbf29765047380898919c5cb56f9"
            .parse()
            .unwrap(),
    );
    assert_eq!(0, app.delivered_txs.len());
    begin_block(&mut app);
    let mut creq = RequestDeliverTx::default();
    let tx = PlainTxAux::new(Tx::new(), TxWitness::new());
    creq.set_tx(tx.encode());
    let cresp = app.deliver_tx(&creq);
    assert_ne!(0, cresp.code);
    assert_eq!(0, app.delivered_txs.len());
    assert_eq!(0, cresp.events.len());
}

fn deliver_valid_tx() -> (
    ChainNodeApp<MockClient>,
    WithdrawUnbondedTx,
    StakedStateOpWitness,
    ResponseDeliverTx,
) {
    let (mut app, txaux, tx) = prepare_app_valid_tx();
    let rewards_pool_remaining_old = app
        .last_state
        .as_ref()
        .unwrap()
        .top_level
        .rewards_pool
        .period_bonus;
    assert_eq!(0, app.delivered_txs.len());
    begin_block(&mut app);
    let mut creq = RequestDeliverTx::default();
    creq.set_tx(txaux.encode());
    let cresp = app.deliver_tx(&creq);
    let rewards_pool_remaining_new = app
        .last_state
        .as_ref()
        .unwrap()
        .top_level
        .rewards_pool
        .period_bonus;
    assert!(rewards_pool_remaining_new > rewards_pool_remaining_old);
    match txaux {
        TxAux::EnclaveTx(TxEnclaveAux::WithdrawUnbondedStakeTx { witness, .. }) => {
            (app, tx, witness, cresp)
        }
        _ => unreachable!("prepare_app_valid_tx should prepare stake withdrawal tx"),
    }
}

#[test]
fn deliver_tx_should_add_tx_events() {
    let (app, tx, _, cresp) = deliver_valid_tx();
    assert_eq!(0, cresp.code);
    assert_eq!(1, app.delivered_txs.len());
    assert_eq!(2, cresp.events.len());

    let valid_tx_event = &cresp.events[0];
    assert_eq!(2, valid_tx_event.attributes.len());
    // the unit test transaction just three outputs: 1 CRO + 1 carson / base unit + the rest
    assert_eq!(
        "0.00000347",
        String::from_utf8(valid_tx_event.attributes[0].value.clone()).unwrap()
    );
    assert_eq!(
        &hex::encode(&tx.id()).as_bytes().to_vec(),
        &valid_tx_event.attributes[1].value
    );

    let staking_event = &cresp.events[1];
    assert_eq!(2, valid_tx_event.attributes.len());
    assert_eq!(
        "0x89aef553a06ab0c3173e79de1ce241a9ed3b992c",
        String::from_utf8(staking_event.attributes[0].value.clone()).unwrap()
    );
    assert_eq!(
        "withdraw",
        String::from_utf8(staking_event.attributes[1].value.clone()).unwrap()
    );
    assert_eq!(
        "[{\"key\":\"Unbonded\",\"value\":\"-9999999999999999999\"}]",
        String::from_utf8(staking_event.attributes[2].value.clone()).unwrap()
    );
}

#[test]
#[should_panic]
#[ignore]
fn delivertx_without_beginblocks_should_panic() {
    // TODO: sanity checks in abci https://github.com/tendermint/rust-abci/issues/49
    let mut app = init_chain_for(
        "0x0e7c045110b8dbf29765047380898919c5cb56f4"
            .parse()
            .unwrap(),
    );
    let creq = RequestDeliverTx::default();
    app.deliver_tx(&creq);
}

#[test]
#[should_panic]
#[ignore]
fn endblock_without_beginblocks_should_panic() {
    // TODO: sanity checks in abci https://github.com/tendermint/rust-abci/issues/49
    let mut app = init_chain_for(
        "0x0e7c045110b8dbf29765047380898919c5cb56f4"
            .parse()
            .unwrap(),
    );
    let creq = RequestEndBlock::default();
    let _cresp = app.end_block(&creq);
}

#[test]
fn endblock_should_change_block_height() {
    let mut app = init_chain_for(
        "0xfe7c045110b8dbf29765047380898919c5cb56f9"
            .parse()
            .unwrap(),
    );
    begin_block(&mut app);
    let mut creq = RequestEndBlock::default();
    creq.set_height(10);
    assert_ne!(
        BlockHeight::new(10),
        app.last_state.as_ref().unwrap().last_block_height
    );
    let cresp = app.end_block(&creq);
    assert_eq!(
        BlockHeight::new(10),
        app.last_state.as_ref().unwrap().last_block_height
    );
    assert_eq!(0, cresp.events.len());
}

#[test]
#[should_panic]
#[ignore]
fn commit_without_beginblocks_should_panic() {
    // TODO: sanity checks in abci https://github.com/tendermint/rust-abci/issues/49
    let mut app = init_chain_for(
        "crms1le7qg5gshrdl99m9q3ecpzvfr8zuk4heu7q420"
            .parse()
            .unwrap(),
    );
    let creq = RequestCommit::default();
    let _cresp = app.commit(&creq);
}

#[test]
fn valid_commit_should_persist() {
    let (mut app, tx, _, _) = deliver_valid_tx();

    let old_app_hash = app.last_state.as_ref().unwrap().last_apphash;
    let mut endreq = RequestEndBlock::default();
    endreq.set_height(10);
    let cresp = app.end_block(&endreq);
    assert_eq!(1, cresp.events.len());
    assert_eq!(1, cresp.events[0].attributes.len());
    assert_eq!(1, app.delivered_txs.len());
    let filter = BlockFilter::try_from(cresp.events[0].attributes[0].value.as_slice())
        .expect("there should be a block filter");

    assert!(filter.check_view_key(&tx.attributes.allowed_view[0].view_key));
    let sample = PublicKey::from_slice(&[
        3, 23, 183, 225, 206, 31, 159, 148, 195, 42, 67, 115, 146, 41, 248, 140, 11, 3, 51, 41,
        111, 180, 110, 143, 114, 134, 88, 73, 198, 174, 52, 184, 78,
    ])
    .expect("sample pk");
    assert!(!filter.check_view_key(&sample));

    assert!(app
        .storage
        .lookup_item(LookupItem::TxSealed, &tx.id())
        .is_none());
    assert!(app
        .storage
        .lookup_item(LookupItem::TxWitness, &tx.id())
        .is_none());
    let persisted_state =
        ChainNodeState::decode(&mut app.storage.get_last_app_state().unwrap().as_slice()).unwrap();
    assert_ne!(BlockHeight::new(10), persisted_state.last_block_height);
    assert_ne!(
        BlockHeight::new(10),
        persisted_state.top_level.rewards_pool.last_block_height
    );
    let cresp = app.commit(&RequestCommit::default());
    assert_eq!(0, app.delivered_txs.len());
    assert!(app
        .storage
        .lookup_item(LookupItem::TxSealed, &tx.id())
        .is_some());
    assert!(app
        .storage
        .lookup_item(LookupItem::TxWitness, &tx.id())
        .is_some());
    assert_eq!(
        BlockHeight::new(10),
        app.last_state.as_ref().unwrap().last_block_height
    );
    assert_eq!(
        BlockHeight::new(10),
        app.last_state
            .as_ref()
            .unwrap()
            .top_level
            .rewards_pool
            .last_block_height
    );
    assert_ne!(old_app_hash, app.last_state.as_ref().unwrap().last_apphash);
    assert_eq!(
        &app.last_state.as_ref().unwrap().last_apphash[..],
        &cresp.data[..]
    );
    assert!(app
        .storage
        .lookup_item(
            LookupItem::TxsMerkle,
            &app.last_state.as_ref().unwrap().last_apphash,
        )
        .is_some());
    // TODO: check account
    let new_utxos = BitVec::from_bytes(
        &app.storage
            .lookup_item(LookupItem::TxMetaSpent, &tx.id())
            .unwrap(),
    );
    assert!(!new_utxos.any());
}

#[test]
fn no_delivered_tx_commit_should_keep_apphash() {
    let mut app = init_chain_for(
        "0xfe7c045110b8dbf29765047380898919c5cb56f9"
            .parse()
            .unwrap(),
    );
    let old_app_hash = app.genesis_app_hash;
    begin_block(&mut app);
    app.end_block(&RequestEndBlock::default());
    let cresp = app.commit(&RequestCommit::default());
    assert_eq!(old_app_hash, app.last_state.as_ref().unwrap().last_apphash);
    assert_eq!(&old_app_hash[..], &cresp.data[..]);
}

#[test]
fn query_should_return_an_account() {
    let addr = "fe7c045110b8dbf29765047380898919c5cb56f9";
    let mut app = init_chain_for(addr.parse().unwrap());
    let mut qreq = RequestQuery::new();
    qreq.data = hex::decode(&addr).unwrap();
    qreq.path = "account".into();
    let qresp = app.query(&qreq);
    let account = StakedState::decode(&mut qresp.value.as_slice()).unwrap();
    assert_eq!(account.address, StakedStateAddress::from_str(addr).unwrap());
}

#[test]
fn staking_query_should_return_an_account() {
    let addr = "fe7c045110b8dbf29765047380898919c5cb56f9";
    let mut app = init_chain_for(addr.parse().unwrap());
    let mut qreq = RequestQuery::new();
    qreq.data = hex::decode(&addr).unwrap();
    qreq.path = "staking".into();
    qreq.prove = true;
    let qresp = app.query(&qreq);
    let mstaking = <Option<StakedState>>::decode(&mut qresp.value.as_slice()).unwrap();
    let mut proof_bytes = qresp.proof.get_ref().ops[0].data.as_slice();
    let _proof = SparseMerkleProof::decode(&mut proof_bytes).unwrap();
    assert_eq!(
        mstaking.unwrap().address,
        StakedStateAddress::from_str(addr).unwrap()
    );
}

fn block_commit_with_check(app: &mut ChainNodeApp<MockClient>, tx: TxAux, block_height: i64) {
    let r = RequestInfo::default();
    let info_1 = app.info(&r);
    let app_last_state_1 = app.last_state.clone().unwrap();
    let app_last_block_height_1 = app_last_state_1.last_block_height.value();
    let app_last_hash_1 = hex::encode(app_last_state_1.last_apphash);
    assert_eq!(info_1.last_block_height as u64, app_last_block_height_1);
    assert_eq!(hex::encode(&info_1.last_block_app_hash), app_last_hash_1);

    let mut creq = RequestCheckTx::default();
    creq.set_tx(tx.encode());
    println!("checktx: {:?}", app.check_tx(&creq));
    println!("beginblock: {:?}", begin_block(app));
    let mut dreq = RequestDeliverTx::default();
    dreq.set_tx(tx.encode());
    println!("delivertx: {:?}", app.deliver_tx(&dreq));
    let mut breq = RequestEndBlock::default();
    breq.set_height(block_height);
    println!("endblock: {:?}", app.end_block(&breq));
    let info_2 = app.info(&r);
    let app_last_state_2 = app.last_state.clone().unwrap();
    let app_last_block_height_2 = app_last_state_2.last_block_height.value();
    let app_last_hash_2 = hex::encode(app_last_state_2.last_apphash);
    // app hash didn't changed
    assert_eq!(info_1.last_block_app_hash, info_2.last_block_app_hash);
    // the uncommited last block height increased, but the one in the app info is not.
    assert_eq!(info_2.last_block_height as u64 + 1, app_last_block_height_2);
    assert_eq!(hex::encode(&info_2.last_block_app_hash), app_last_hash_2);
    assert_eq!(
        hex::encode(&info_2.last_block_app_hash),
        hex::encode(&info_1.last_block_app_hash)
    );
    // commit block, app hash changed during commit
    println!("commit: {:?}", app.commit(&RequestCommit::default()));
    // after commit, block height increased, and app_hash changed
    let info_3 = app.info(&r);
    let app_last_state_3 = app.last_state.clone().unwrap();
    let app_last_block_height_3 = app_last_state_3.last_block_height.value();
    let app_last_hash_3 = hex::encode(app_last_state_3.last_apphash);
    assert_eq!(hex::encode(&info_3.last_block_app_hash), app_last_hash_3);
    // app hash changed
    assert_ne!(info_2.last_block_app_hash, info_3.last_block_app_hash);
    assert_eq!(info_3.last_block_height as u64, app_last_block_height_3);
}
pub fn get_account(
    account_address: &RedeemAddress,
    app: &ChainNodeApp<MockClient>,
) -> Option<StakedState> {
    app.staking_getter(BufferType::Consensus)
        .get(&StakedStateAddress::BasicRedeem(*account_address))
}

fn get_tx_meta(txid: &TxId, app: &ChainNodeApp<MockClient>) -> BitVec {
    BitVec::from_bytes(
        &app.storage
            .lookup_item(LookupItem::TxMetaSpent, txid)
            .unwrap(),
    )
}

#[test]
#[allow(clippy::cognitive_complexity)]
fn all_valid_tx_types_should_commit() {
    let secp = secp256k1::SECP256K1;
    let secret_key = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
    let x_public_key = XOnlyPublicKey::from_secret_key(&secp, &secret_key);
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);
    let addr = RedeemAddress::from(&public_key);

    let secret_key2 = SecretKey::from_slice(&[0xce; 32]).expect("32 bytes, within curve order");
    let public_key2 = PublicKey::from_secret_key(&secp, &secret_key2);
    // addr2 is not exist in genesis.
    let addr2 = RedeemAddress::from(&public_key2);

    let mut app = init_chain_for(addr);

    let merkle_tree = MerkleTree::new(vec![RawXOnlyPubkey::from(x_public_key.serialize())]);

    let eaddr = ExtendedAddr::OrTree(merkle_tree.root_hash());
    let tx0 = WithdrawUnbondedTx::new(
        0,
        vec![
            TxOut::new_with_timelock(eaddr.clone(), Coin::one(), DEFAULT_GENESIS_TIME),
            TxOut::new_with_timelock(
                eaddr.clone(),
                (Coin::one() + Coin::one()).unwrap(),
                DEFAULT_GENESIS_TIME,
            ),
            TxOut::new_with_timelock(eaddr.clone(), Coin::one(), DEFAULT_GENESIS_TIME),
            TxOut::new_with_timelock(
                eaddr.clone(),
                Coin::new(9999999999599999602).unwrap(),
                DEFAULT_GENESIS_TIME,
            ), // rest
        ],
        TxAttributes::new_with_access(0, vec![TxAccessPolicy::new(public_key, TxAccess::AllData)]),
    );
    let txid = &tx0.id();
    let witness0 = StakedStateOpWitness::new(get_ecdsa_witness(&secp, &txid, &secret_key));
    let withdrawtx = TxAux::EnclaveTx(TxEnclaveAux::WithdrawUnbondedStakeTx {
        no_of_outputs: tx0.outputs.len() as TxoSize,
        witness: witness0,
        payload: encrypt(&PlainTxAux::WithdrawUnbondedStakeTx(tx0.clone()), tx0.id()),
    });
    {
        let account = get_account(&addr, &app).expect("acount not exist");
        // TODO: more precise amount assertions
        assert!(account.unbonded > Coin::zero());
        assert_eq!(account.nonce, 0);
    }
    block_commit_with_check(&mut app, withdrawtx, 1);

    {
        let account = get_account(&addr, &app).expect("acount not exist");
        assert_eq!(account.unbonded, Coin::zero());
        assert_eq!(account.nonce, 1);
        let spend_utxos = get_tx_meta(&txid, &app);
        assert!(!spend_utxos.any());
    }
    let utxo1 = TxoPointer::new(*txid, 0);
    let mut tx1 = Tx::new();
    tx1.add_input(utxo1);
    tx1.add_output(TxOut::new(eaddr, Coin::from(99999700u32)));
    let txid1 = tx1.id();
    let witness1 = vec![TxInWitness::TreeSig(
        schnorr_sign(
            &secp,
            &Message::from_slice(&txid1).unwrap(),
            &secret_key,
            &mut rand::thread_rng(),
        ),
        merkle_tree
            .generate_proof(RawXOnlyPubkey::from(x_public_key.serialize()))
            .unwrap(),
    )]
    .into();
    let plain_txaux = PlainTxAux::TransferTx(tx1.clone(), witness1);
    let transfertx = TxAux::EnclaveTx(TxEnclaveAux::TransferTx {
        inputs: tx1.inputs.clone(),
        no_of_outputs: tx1.outputs.len() as TxoSize,
        payload: encrypt(&plain_txaux, tx1.id()),
    });
    {
        let spent_utxos = get_tx_meta(&txid, &app);
        assert!(!spent_utxos.any());
    }
    block_commit_with_check(&mut app, transfertx, 2);
    {
        let spent_utxos0 = get_tx_meta(&txid, &app);
        assert!(spent_utxos0[0] && !spent_utxos0[1]);
        let spent_utxos1 = get_tx_meta(&txid1, &app);
        assert!(!spent_utxos1.any());
    }
    let utxo2 = TxoPointer::new(*txid, 1);
    let tx2 = DepositBondTx::new(vec![utxo2], addr.into(), StakedStateOpAttributes::new(0));
    let witness2 = vec![TxInWitness::TreeSig(
        schnorr_sign(
            &secp,
            &Message::from_slice(&tx2.id()).unwrap(),
            &secret_key,
            &mut rand::thread_rng(),
        ),
        merkle_tree
            .generate_proof(RawXOnlyPubkey::from(x_public_key.serialize()))
            .unwrap(),
    )]
    .into();
    let depositx = TxAux::EnclaveTx(TxEnclaveAux::DepositStakeTx {
        tx: tx2.clone(),
        payload: encrypt(&PlainTxAux::DepositStakeTx(witness2), tx2.id()),
    });
    {
        let spent_utxos0 = get_tx_meta(&txid, &app);
        assert!(spent_utxos0[0] && !spent_utxos0[1]);
        let account = get_account(&addr, &app).expect("acount not exist");
        assert_eq!(account.bonded, Coin::zero());
        assert_eq!(account.nonce, 1);
    }
    block_commit_with_check(&mut app, depositx, 3);
    {
        let spent_utxos0 = get_tx_meta(&txid, &app);
        assert!(spent_utxos0[0] && spent_utxos0[1]);
        let account = get_account(&addr, &app).expect("acount not exist");
        // TODO: more precise amount assertions
        assert!(account.bonded > Coin::zero());
        assert_eq!(account.nonce, 1);
    }

    let utxo3 = TxoPointer::new(*txid, 2);
    let tx3 = DepositBondTx::new(vec![utxo3], addr2.into(), StakedStateOpAttributes::new(0));
    let witness3 = vec![TxInWitness::TreeSig(
        schnorr_sign(
            &secp,
            &Message::from_slice(&tx3.id()).unwrap(),
            &secret_key,
            &mut rand::thread_rng(),
        ),
        merkle_tree
            .generate_proof(RawXOnlyPubkey::from(x_public_key.serialize()))
            .unwrap(),
    )]
    .into();
    let depositx = TxAux::EnclaveTx(TxEnclaveAux::DepositStakeTx {
        tx: tx3.clone(),
        payload: encrypt(&PlainTxAux::DepositStakeTx(witness3), tx3.id()),
    });
    {
        let spent_utxos0 = get_tx_meta(txid, &app);
        assert!(spent_utxos0[0] && spent_utxos0[1] && !spent_utxos0[2]);
        let account = get_account(&addr2, &app);
        assert!(account.is_none());
    }
    block_commit_with_check(&mut app, depositx, 4);
    {
        let spent_utxos0 = get_tx_meta(txid, &app);
        assert!(spent_utxos0[0] && spent_utxos0[1] && spent_utxos0[2]);
        let account = get_account(&addr2, &app).expect("account not exist");
        // TODO: more precise amount assertions
        assert!(account.bonded > Coin::zero());
        assert_eq!(account.nonce, 0);
    }

    let tx = NodeJoinRequestTx::new(
        1,
        addr.into(),
        StakedStateOpAttributes::new(0),
        mock_council_node_join(TendermintValidatorPubKey::Ed25519([2u8; 32])),
    );
    let secp = secp256k1::SECP256K1;
    let witness = StakedStateOpWitness::new(get_ecdsa_witness(&secp, &tx.id(), &secret_key));
    let nodejointx = TxAux::PublicTx(TxPublicAux::NodeJoinTx(tx, witness));
    {
        let account = get_account(&addr, &app).expect("account not exist");
        assert!(account.node_meta.is_none());
        assert_eq!(
            app.last_state
                .as_ref()
                .unwrap()
                .staking_table
                .get_chosen_validators()
                .len(),
            1
        );
        assert_eq!(account.nonce, 1);
    }
    block_commit_with_check(&mut app, nodejointx, 5);
    {
        let account = get_account(&addr, &app).expect("account not exist");
        assert!(account.node_meta.is_some());
        assert_eq!(
            app.last_state
                .as_ref()
                .unwrap()
                .staking_table
                .get_chosen_validators()
                .len(),
            2
        );
        assert_eq!(account.nonce, 2);
    }

    let tx4 = UnbondTx::new(
        addr.into(),
        2,
        Coin::unit(),
        StakedStateOpAttributes::new(0),
    );
    let witness4 = StakedStateOpWitness::new(get_ecdsa_witness(&secp, &tx4.id(), &secret_key));
    let unbondtx = TxAux::PublicTx(TxPublicAux::UnbondStakeTx(tx4, witness4));
    let bonded = {
        let account = get_account(&addr, &app).expect("account not exist");
        assert_eq!(account.unbonded, Coin::zero());
        assert_eq!(account.nonce, 2);
        account.bonded
    };
    block_commit_with_check(&mut app, unbondtx, 6);
    {
        let account = get_account(&addr, &app).expect("account not exist");
        assert_eq!(account.unbonded, Coin::unit());
        assert_eq!(account.nonce, 3);
        // fee is non zero
        assert!(
            account.bonded < (bonded - Coin::unit()).unwrap(),
            "bonded should subtract fee amount"
        );
    }
}

#[test]
fn query_should_return_proof_for_committed_tx() {
    let (env, storage) =
        ChainEnv::new_with_customizer(Coin::max(), Coin::zero(), 2, |parameters| {
            parameters.required_council_node_stake = (Coin::max() / 10).unwrap();
        });
    let mut app = env.chain_node(storage);
    let _rsp = app.init_chain(&env.req_init_chain());

    app.begin_block(&env.req_begin_block(1, 0));

    let tx_aux = env.unbond_tx(Coin::new(5000000000000000000).unwrap(), 0, 0);
    let rsp_tx = app.deliver_tx(&RequestDeliverTx {
        tx: tx_aux.encode(),
        ..Default::default()
    });

    assert_eq!(0, rsp_tx.code);

    let _response_end_block = app.end_block(&RequestEndBlock {
        height: 1,
        ..Default::default()
    });
    let cresp = app.commit(&RequestCommit::default());
    let mut qreq = RequestQuery::new();
    qreq.data = tx_aux.tx_id().to_vec();
    qreq.path = "store".into();
    qreq.prove = true;
    let qresp = app.query(&qreq);
    let returned_tx = UnbondTx::decode(&mut qresp.value.as_slice()).unwrap();
    match &tx_aux {
        TxAux::PublicTx(TxPublicAux::UnbondStakeTx(stx, _)) => {
            assert_eq!(returned_tx, stx.clone());
        }
        _ => unreachable!(),
    }

    let proof = qresp.proof.unwrap();
    let merkle = MerkleTree::new(vec![tx_aux.tx_id()]);
    assert_eq!(proof.ops.len(), 2);

    let mut transaction_root_hash = [0u8; 32];
    transaction_root_hash.copy_from_slice(proof.ops[0].key.as_slice());

    let mut transaction_proof_data = proof.ops[0].data.as_slice();
    let transaction_proof = <Proof<H256>>::decode(&mut transaction_proof_data).unwrap();

    assert!(transaction_proof.verify(&transaction_root_hash));
    assert_eq!(merkle.root_hash(), transaction_root_hash);
    let last_state = app.last_state.clone().unwrap();
    assert_eq!(
        compute_app_hash(
            &merkle,
            &last_state.top_level.account_root,
            &last_state.top_level.rewards_pool,
            &last_state.top_level.network_params
        )
        .to_vec(),
        cresp.data
    );
    let mut qreq2 = RequestQuery::new();
    qreq2.data = tx_aux.tx_id().to_vec();
    qreq2.path = "witness".into();
    let qresp = app.query(&qreq2);
    match &tx_aux {
        TxAux::PublicTx(TxPublicAux::UnbondStakeTx(_, witness)) => {
            assert_eq!(qresp.value, witness.encode());
        }
        _ => unreachable!(),
    }
    let witness_hash: [u8; 32] = blake3::hash(&qresp.value).into();
    assert_eq!(proof.ops[1].data, witness_hash.to_vec());
}

#[test]
#[should_panic]
fn check_invalid_punishment_config() {
    ChainEnv::new_with_customizer(Coin::max(), Coin::zero(), 2, |parameters| {
        parameters.jailing_config.missed_block_threshold = 720;
        parameters.jailing_config.block_signing_window = 360;
    });
}
