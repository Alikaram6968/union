use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use cometbft_rpc::rpc_types::{GrpcAbciQueryError, TxResponse};
use cosmwasm_std::Addr;
use ibc_union_ucs03_zkgm::msg::TokenMinterInitMsg;
use protos::{
    cosmos::base::abci,
    cosmwasm::wasm::v1::{
        MsgInstantiateContract2, MsgInstantiateContract2Response, MsgMigrateContract,
        MsgMigrateContractResponse, MsgStoreCode, MsgStoreCodeResponse,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Digest;
use tracing::{debug, info, instrument};
use tracing_subscriber::EnvFilter;
use unionlabs::{
    bech32::Bech32,
    cosmos::{
        auth::base_account::BaseAccount,
        base::{abci::gas_info::GasInfo, coin::Coin},
        crypto::{secp256k1, AnyPubKey},
        tx::{
            auth_info::AuthInfo, fee::Fee, mode_info::ModeInfo, sign_doc::SignDoc,
            signer_info::SignerInfo, signing::sign_info::SignMode, tx::Tx, tx_body::TxBody,
            tx_raw::TxRaw,
        },
    },
    encoding::{EncodeAs, Proto},
    google::protobuf::any::Any,
    primitives::{Bytes, H256},
    prost::{Message, Name},
    signer::CosmosSigner,
};

#[derive(clap::Parser)]
enum App {
    DeployFull {
        #[arg(long)]
        rpc_url: String,
        #[arg(long)]
        private_key: H256,
        #[arg(long)]
        contracts: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[command(flatten)]
        gas_config: GasConfig,
    },
    Addresses {
        #[arg(long)]
        bech32_prefix: String,
        #[arg(long)]
        private_key: H256,
        #[arg(long)]
        lightclient: Vec<String>,
        #[command(flatten)]
        apps: AppFlags,
        #[arg(long)]
        output: PathBuf,
    },
    #[clap(subcommand)]
    Tx(TxCmd),
    #[clap(subcommand)]
    Query(QueryCmd),
    Instantiate2Address {
        #[arg(long)]
        bech32_prefix: String,
        #[arg(long)]
        private_key: H256,
        #[arg(long)]
        checksum: H256,
        #[arg(long)]
        salt: String,
    },
}

#[derive(clap::Subcommand)]
enum TxCmd {
    StoreCode {
        #[arg(long)]
        private_key: H256,
        #[arg(long)]
        rpc_url: String,
        #[arg(long)]
        bytecode_path: String,
        #[command(flatten)]
        gas_config: GasConfig,
    },
    Instantiate2 {
        #[arg(long)]
        private_key: H256,
        #[arg(long)]
        rpc_url: String,
        #[arg(long)]
        code_id: u64,
        #[arg(long)]
        salt: String,
        #[arg(long)]
        msg: String,
        #[command(flatten)]
        gas_config: GasConfig,
    },
}

#[derive(Debug, Clone, PartialEq, Default, clap::Args)]
pub struct AppFlags {
    #[arg(long)]
    ucs00: bool,
    #[arg(long)]
    ucs03: bool,
}

#[derive(clap::Subcommand)]
enum QueryCmd {
    CodeInfo {
        #[arg(long)]
        rpc_url: String,
        #[arg(long)]
        code_id: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    do_main().await
}

const BYTECODE_BASE_BYTECODE: &[u8] = &hex_literal::hex!("0061736d0100000001110360037f7f7f017f60017f017f60017f000304030001020503010001074605066d656d6f7279020013696e746572666163655f76657273696f6e5f3800000b696e7374616e7469617465000008616c6c6f6361746500010a6465616c6c6f6361746500020a0f03040041330b0400413f0b0300010b0b4e010041010b487b226f6b223a7b226d65737361676573223a5b5d2c2261747472696275746573223a5b5d2c226576656e7473223a5b5d7d7d0100000032000000320000004b000000000200000002");

fn sha2(bz: impl AsRef<[u8]>) -> H256 {
    ::sha2::Sha256::new().chain_update(bz).finalize().into()
}

const CORE: &str = "core";
const LIGHTCLIENT: &str = "lightclient";
const APP: &str = "app";
const UCS03: &str = "ucs03";

const BYTECODE_BASE: &str = "bytecode-base";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct ContractPaths {
    core: PathBuf,
    lightclient: BTreeMap<String, PathBuf>,
    app: AppPaths,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct AppPaths {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ucs00: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ucs03: Option<Ucs03Config>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Ucs03Config {
    path: PathBuf,
    token_minter_path: PathBuf,
    token_minter_config: TokenMinterConfig,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum TokenMinterConfig {
    Cw20 { cw20_base: PathBuf },
    Native,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct ContractAddresses {
    core: String,
    lightclient: BTreeMap<String, String>,
    app: AppAddresses,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct AppAddresses {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ucs00: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ucs03: Option<String>,
}

async fn do_main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let app = App::parse();

    match app {
        App::Addresses {
            bech32_prefix,
            private_key,
            lightclient: lightclients,
            apps,
            output,
        } => {
            let signer = CosmosSigner::new(
                bip32::secp256k1::ecdsa::SigningKey::from_bytes(&private_key.into())
                    .expect("invalid private key"),
                bech32_prefix,
            );

            let core = instantiate2_address(
                signer.to_string().parse().unwrap(),
                sha2(BYTECODE_BASE_BYTECODE),
                CORE,
            )
            .unwrap()
            .to_string();
            let lightclient = lightclients
                .into_iter()
                .map(|salt| {
                    (
                        salt.clone(),
                        instantiate2_address(
                            signer.to_string().parse().unwrap(),
                            sha2(BYTECODE_BASE_BYTECODE),
                            &format!("{LIGHTCLIENT}/{salt}"),
                        )
                        .unwrap()
                        .to_string(),
                    )
                })
                .collect();

            let mut app = AppAddresses::default();

            if apps.ucs00 {
                todo!()
            }

            if apps.ucs03 {
                app.ucs03 = Some(
                    instantiate2_address(
                        signer.to_string().parse().unwrap(),
                        sha2(BYTECODE_BASE_BYTECODE),
                        &format!("{APP}/{UCS03}"),
                    )
                    .unwrap()
                    .to_string(),
                );
            }

            let contract_addresses = ContractAddresses {
                core,
                lightclient,
                app,
            };

            std::fs::write(output, serde_json::to_string(&contract_addresses).unwrap())?;
        }
        App::DeployFull {
            rpc_url,
            private_key,
            contracts,
            output,
            gas_config,
        } => {
            let contracts = serde_json::from_slice::<ContractPaths>(
                &std::fs::read(contracts).context("reading contracts path")?,
            )?;

            let ctx = Ctx::new(rpc_url, private_key, gas_config).await?;

            let bytecode_base_address = ctx
                .instantiate2_address(sha2(BYTECODE_BASE_BYTECODE), BYTECODE_BASE)
                .await?;

            let bytecode_base_contract = ctx.contract_info(bytecode_base_address.clone()).await?;

            // dbg!(&bytecode_base_contract);

            let bytecode_base_code_id = match bytecode_base_contract {
                Some(_) => ctx
                    .instantiate_code_id_of_contract(bytecode_base_address)
                    .await?
                    .unwrap(),
                // contract does not exist on chain
                None => {
                    let (_, response) = ctx
                        .tx::<_, MsgStoreCodeResponse>(MsgStoreCode {
                            sender: ctx.signer.to_string(),
                            wasm_byte_code: BYTECODE_BASE_BYTECODE.to_vec(),
                            ..Default::default()
                        })
                        .await
                        .context("store code")?;

                    ctx.tx::<_, MsgInstantiateContract2Response>(MsgInstantiateContract2 {
                        sender: ctx.signer.to_string(),
                        admin: ctx.signer.to_string(),
                        code_id: response.code_id,
                        label: BYTECODE_BASE.to_string(),
                        msg: b"{}".to_vec(),
                        salt: BYTECODE_BASE.as_bytes().to_vec(),
                        ..Default::default()
                    })
                    .await
                    .context("instantiate2")?;

                    response.code_id
                }
            };

            println!("{bytecode_base_code_id}");

            let core_address = ctx
                .deploy_and_initiate(
                    std::fs::read(contracts.core)?,
                    bytecode_base_code_id,
                    ibc_union_msg::msg::InitMsg {},
                    CORE.to_owned(),
                )
                .await?;

            let mut contract_addresses = ContractAddresses {
                core: core_address.clone(),
                lightclient: BTreeMap::default(),
                app: AppAddresses {
                    ucs00: None,
                    ucs03: None,
                },
            };

            for (salt, path) in contracts.lightclient {
                let address = ctx
                    .deploy_and_initiate(
                        std::fs::read(path)?,
                        bytecode_base_code_id,
                        ibc_union_light_client::msg::InitMsg {
                            ibc_host: Addr::unchecked(core_address.clone()),
                        },
                        format!("{LIGHTCLIENT}/{salt}"),
                    )
                    .await?;

                contract_addresses.lightclient.insert(salt, address);
            }

            if let Some(_ucs00) = contracts.app.ucs00 {}

            if let Some(ucs03_config) = contracts.app.ucs03 {
                let (tx_hash, response) = ctx
                    .tx::<_, MsgStoreCodeResponse>(MsgStoreCode {
                        sender: ctx.signer.to_string(),
                        wasm_byte_code: std::fs::read(ucs03_config.token_minter_path)?,
                        ..Default::default()
                    })
                    .await
                    .context("store minter code")?;

                let code_id = response.code_id;

                info!(%tx_hash, code_id, "minter stored");

                let minter_init_msg = match ucs03_config.token_minter_config {
                    TokenMinterConfig::Cw20 { cw20_base } => {
                        let (tx_hash, response) = ctx
                            .tx::<_, MsgStoreCodeResponse>(MsgStoreCode {
                                sender: ctx.signer.to_string(),
                                wasm_byte_code: std::fs::read(cw20_base)?,
                                ..Default::default()
                            })
                            .await
                            .context("store minter code")?;

                        let code_id = response.code_id;

                        info!(%tx_hash, code_id, "cw20-base stored");

                        TokenMinterInitMsg::Cw20 {
                            cw20_base_code_id: code_id,
                        }
                    }
                    TokenMinterConfig::Native => TokenMinterInitMsg::Native,
                };

                let address = ctx
                    .deploy_and_initiate(
                        std::fs::read(ucs03_config.path)?,
                        bytecode_base_code_id,
                        ibc_union_ucs03_zkgm::msg::InitMsg {
                            config: ibc_union_ucs03_zkgm::msg::Config {
                                // no constructors ffs
                                ibc_host: unsafe { std::mem::transmute(core_address.clone()) },
                                token_minter_code_id: code_id,
                            },
                            minter_init_msg,
                        },
                        format!("{APP}/{UCS03}"),
                    )
                    .await?;

                contract_addresses.app.ucs03 = Some(address);
            }

            std::fs::write(output, serde_json::to_string(&contract_addresses).unwrap())?;
        }
        App::Tx(tx_cmd) => match tx_cmd {
            TxCmd::StoreCode {
                private_key,
                rpc_url,
                bytecode_path,
                gas_config,
            } => {
                let wasm_byte_code =
                    std::fs::read(bytecode_path).context("reading bytecode path")?;

                let ctx = Ctx::new(rpc_url, private_key, gas_config).await?;

                let (tx_hash, response) = ctx
                    .tx::<_, MsgStoreCodeResponse>(MsgStoreCode {
                        sender: ctx.signer.to_string(),
                        wasm_byte_code: wasm_byte_code.to_vec(),
                        ..Default::default()
                    })
                    .await
                    .context("store code")?;

                println!(
                    "{}",
                    json!({ "tx_hash": tx_hash, "code_id": response.code_id })
                )
            }
            TxCmd::Instantiate2 {
                private_key,
                rpc_url,
                code_id,
                salt,
                msg,
                gas_config,
            } => {
                let config = Ctx::new(rpc_url, private_key, gas_config).await?;

                let (tx_hash, response) = config
                    .tx::<_, MsgInstantiateContract2Response>(MsgInstantiateContract2 {
                        sender: config.signer.to_string(),
                        admin: config.signer.to_string(),
                        code_id,
                        label: salt.to_string(),
                        msg: msg.into_bytes(),
                        salt: salt.as_bytes().to_vec(),
                        ..Default::default()
                    })
                    .await
                    .context("instantiate2")?;

                println!(
                    "{}",
                    json!({ "tx_hash": tx_hash, "address": response.address })
                )
            }
        },
        App::Query(query_cmd) => match query_cmd {
            QueryCmd::CodeInfo { rpc_url, code_id } => {
                let client = cometbft_rpc::Client::new(rpc_url).await?;

                let response = client
                    .grpc_abci_query::<_, protos::cosmwasm::wasm::v1::QueryCodeInfoResponse>(
                        "/cosmwasm.wasm.v1.Query/CodeInfo",
                        &(protos::cosmwasm::wasm::v1::QueryCodeInfoRequest { code_id }),
                        None,
                        false,
                    )
                    .await?
                    .into_result()?
                    .unwrap();

                println!(
                    "{}",
                    json!({
                        "creator": response.creator,
                        "checksum": <H256>::try_from(response.checksum)?,
                    })
                )
            }
        },
        App::Instantiate2Address {
            bech32_prefix,
            private_key,
            checksum,
            salt,
        } => {
            let signer = CosmosSigner::new(
                bip32::secp256k1::ecdsa::SigningKey::from_bytes(&private_key.into())
                    .expect("invalid private key"),
                bech32_prefix,
            );

            let bech32 = signer.to_string().parse::<Bech32<Vec<u8>>>().unwrap();

            let addr = cosmwasm_std::instantiate2_address(
                checksum.get(),
                &bech32.data().as_slice().into(),
                salt.as_bytes(),
            )?;

            println!("{}", Bech32::new(bech32.hrp(), &*addr))
        }
    }

    Ok(())
}

struct Ctx {
    signer: CosmosSigner,
    client: cometbft_rpc::Client,
    gas_config: GasConfig,
    chain_id: String,
}

#[derive(Debug, Clone, PartialEq, Default, clap::Args)]
pub struct GasConfig {
    #[arg(long)]
    pub gas_price: f64,
    #[arg(long)]
    pub gas_denom: String,
    #[arg(long)]
    pub gas_multiplier: f64,
    #[arg(long)]
    pub max_gas: u64,
    #[arg(long, default_value_t = 0)]
    pub min_gas: u64,
}

impl GasConfig {
    pub fn mk_fee(&self, gas: u64) -> Fee {
        // gas limit = provided gas * multiplier, clamped between min_gas and max_gas
        let gas_limit = u128_saturating_mul_f64(gas.into(), self.gas_multiplier)
            .clamp(self.min_gas.into(), self.max_gas.into());

        let amount = u128_saturating_mul_f64(gas.into(), self.gas_price);

        Fee {
            amount: vec![Coin {
                amount,
                denom: self.gas_denom.clone(),
            }],
            gas_limit: gas_limit.try_into().unwrap_or(u64::MAX),
            payer: String::new(),
            granter: String::new(),
        }
    }
}

fn u128_saturating_mul_f64(u: u128, f: f64) -> u128 {
    (num_rational::BigRational::from_integer(u.into())
        * num_rational::BigRational::from_float(f).expect("finite"))
    .to_integer()
    .try_into()
    .unwrap_or(u128::MAX)
    // .expect("overflow")
}

impl Ctx {
    async fn new(rpc_url: String, private_key: H256, gas_config: GasConfig) -> Result<Ctx> {
        let client = cometbft_rpc::Client::new(rpc_url)
            .await
            .context("creating cometbft rpc client")?;

        let prefix = client
            .grpc_abci_query::<_, protos::cosmos::auth::v1beta1::Bech32PrefixResponse>(
                "/cosmos.auth.v1beta1.Query/Bech32Prefix",
                &protos::cosmos::auth::v1beta1::Bech32PrefixRequest {},
                None,
                false,
            )
            .await
            .context("querying bech32 prefix")?
            .into_result()?
            .unwrap()
            .bech32_prefix;

        let chain_id = client
            .status()
            .await
            .context("querying node status")?
            .node_info
            .network;

        let ctx = Ctx {
            signer: CosmosSigner::new(
                bip32::secp256k1::ecdsa::SigningKey::from_bytes(&private_key.into())
                    .expect("invalid private key"),
                prefix,
            ),
            client,
            gas_config,
            chain_id,
        };

        Ok(ctx)
    }

    async fn tx<M: Message + Name, R: Message + Default + Name>(
        &self,
        msg: M,
    ) -> Result<(H256, R)> {
        let (tx_hash, result) = self
            .broadcast_tx_commit([protos::google::protobuf::Any {
                type_url: M::type_url(),
                value: msg.encode_to_vec().into(),
            }])
            .await
            .context("broadcast_tx_commit")?;

        let response =
            <abci::v1beta1::TxMsgData as Message>::decode(&*result.tx_result.data.unwrap())
                .unwrap();

        assert_eq!(&*response.msg_responses[0].type_url, R::type_url());

        let response =
            R::decode(&*response.msg_responses[0].value).context("parsing returned address")?;

        Ok((tx_hash, response))
    }

    async fn contract_info(
        &self,
        address: String,
    ) -> Result<Option<protos::cosmwasm::wasm::v1::ContractInfo>> {
        let result = self
            .client
            .grpc_abci_query::<_, protos::cosmwasm::wasm::v1::QueryContractInfoResponse>(
                "/cosmwasm.wasm.v1.Query/ContractInfo",
                &(protos::cosmwasm::wasm::v1::QueryContractInfoRequest { address }),
                None,
                false,
            )
            .await?
            .into_result();

        match result {
            Ok(ok) => Ok(Some(ok.unwrap().contract_info.unwrap())),
            Err(err) => {
                if err.error_code.get() == 6 && err.codespace == "sdk" {
                    Ok(None)
                } else {
                    Err(err.into())
                }
            }
        }
    }

    // async fn code_info(&self, code_id: u64) -> Result<Option<H256>> {
    //     let result = self
    //         .client
    //         .grpc_abci_query::<_, protos::cosmwasm::wasm::v1::QueryCodeInfoResponse>(
    //             "/cosmwasm.wasm.v1.Query/CodeInfo",
    //             &protos::cosmwasm::wasm::v1::QueryCodeInfoRequest { code_id },
    //             None,
    //             false,
    //         )
    //         .await?
    //         .into_result();

    //     match result {
    //         Ok(ok) => Ok(Some(ok.unwrap().checksum.try_into().unwrap())),
    //         Err(err) => {
    //             // if err.error_code.get() == 6 && err.codespace == "sdk" {
    //             //     Ok(None)
    //             // } else {
    //             Err(err.into())
    //             // }
    //         }
    //     }
    // }

    async fn instantiate_code_id_of_contract(&self, address: String) -> Result<Option<u64>> {
        let result = self.contract_history(address).await?;

        match result {
            Ok(ok) => {
                let contract_code_history_entry = &ok.unwrap().entries[0];

                if contract_code_history_entry.operation
                    != protos::cosmwasm::wasm::v1::ContractCodeHistoryOperationType::Init as i32
                {
                    bail!(
                        "invalid state {} for first history entry",
                        contract_code_history_entry.operation
                    )
                }

                Ok(Some(contract_code_history_entry.code_id))
            }
            Err(err) => {
                // if err.error_code.get() == 6 && err.codespace == "sdk" {
                //     Ok(None)
                // } else {
                Err(err.into())
                // }
            }
        }
    }

    async fn contract_history(
        &self,
        address: String,
    ) -> Result<
        Result<
            Option<protos::cosmwasm::wasm::v1::QueryContractHistoryResponse>,
            GrpcAbciQueryError,
        >,
    > {
        Ok(self
            .client
            .grpc_abci_query::<_, protos::cosmwasm::wasm::v1::QueryContractHistoryResponse>(
                "/cosmwasm.wasm.v1.Query/ContractHistory",
                &protos::cosmwasm::wasm::v1::QueryContractHistoryRequest {
                    address,
                    ..Default::default()
                },
                None,
                false,
            )
            .await?
            .into_result())
    }

    async fn instantiate2_address(&self, checksum: H256, salt: &str) -> Result<String> {
        let bech32 = self.signer.to_string().parse::<Bech32<Vec<u8>>>().unwrap();

        let addr = cosmwasm_std::instantiate2_address(
            checksum.get(),
            &bech32.data().as_slice().into(),
            salt.as_bytes(),
        )?;

        Ok(Bech32::new(bech32.hrp(), &*addr).to_string())
    }

    #[instrument(skip_all, fields(%salt))]
    async fn deploy_and_initiate(
        &self,
        wasm_byte_code: Vec<u8>,
        bytecode_base_code_id: u64,
        msg: impl Serialize,
        salt: String,
    ) -> Result<String, anyhow::Error> {
        let address = self
            .instantiate2_address(sha2(BYTECODE_BASE_BYTECODE), &salt)
            .await?;

        info!("{salt} address is {address}");

        let contract_info = self.contract_info(address.clone()).await?;

        let do_instantiate = match contract_info {
            Some(_) => {
                let contract_history = self.contract_history(address.clone()).await??.unwrap();
                match contract_history.entries.len().cmp(&1) {
                    std::cmp::Ordering::Less => panic!("impossible"),
                    std::cmp::Ordering::Equal => {
                        info!(
                            "contract {address} ({salt}) has already been stored but not yet migrated"
                        );
                        false
                    }
                    std::cmp::Ordering::Greater => {
                        info!("contract {address} ({salt}) has already been stored and migrated");
                        return Ok(address);
                    }
                }
            }
            None => true,
        };

        if do_instantiate {
            let (_, instantiate2_response) = self
                .tx::<_, MsgInstantiateContract2Response>(MsgInstantiateContract2 {
                    sender: self.signer.to_string(),
                    admin: self.signer.to_string(),
                    code_id: bytecode_base_code_id,
                    label: salt.clone(),
                    msg: json!({}).to_string().into_bytes(),
                    salt: salt.into_bytes(),
                    ..Default::default()
                })
                .await
                .context("instantiate2")?;

            assert_eq!(address, instantiate2_response.address);
        }

        let (tx_hash, store_code_response) = self
            .tx::<_, MsgStoreCodeResponse>(MsgStoreCode {
                sender: self.signer.to_string(),
                wasm_byte_code,
                ..Default::default()
            })
            .await
            .context("store code")?;

        info!(
            %tx_hash,
            code_id = store_code_response.code_id,
        );

        let (_, _migrate_response) = self
            .tx::<_, MsgMigrateContractResponse>(MsgMigrateContract {
                sender: self.signer.to_string(),
                contract: address.clone(),
                code_id: store_code_response.code_id,
                msg: json!({ "init": msg }).to_string().into_bytes(),
            })
            .await
            .context("init")?;

        // info!(%tx_hash, );

        Ok(address)
    }

    /// - simulate tx
    /// - submit tx
    /// - wait for inclusion
    /// - return (tx_hash, gas_used)
    pub async fn broadcast_tx_commit(
        &self,
        messages: impl IntoIterator<Item = protos::google::protobuf::Any> + Clone,
    ) -> Result<(H256, TxResponse)> {
        let account = self
            .account_info(&self.signer.to_string())
            .await
            .context("fetching account info")?;

        let (tx_body, mut auth_info, simulation_gas_info) =
            self.simulate_tx(messages).await.context("simulate_tx")?;

        info!(
            gas_used = %simulation_gas_info.gas_used,
            gas_wanted = %simulation_gas_info.gas_wanted,
            "tx simulation successful"
        );

        auth_info.fee = self.gas_config.mk_fee(simulation_gas_info.gas_used);

        info!(
            fee = %auth_info.fee.amount[0].amount,
            gas_multiplier = %self.gas_config.gas_multiplier,
            "submitting transaction with gas"
        );

        // re-sign the new auth info with the simulated gas
        let signature = self
            .signer
            .try_sign(
                &SignDoc {
                    body_bytes: tx_body.clone().encode_as::<Proto>(),
                    auth_info_bytes: auth_info.clone().encode_as::<Proto>(),
                    chain_id: self.chain_id.to_string(),
                    account_number: account.account_number,
                }
                .encode_as::<Proto>(),
            )
            .expect("signing failed")
            .to_bytes()
            .to_vec();

        let tx_raw_bytes = TxRaw {
            body_bytes: tx_body.clone().encode_as::<Proto>(),
            auth_info_bytes: auth_info.clone().encode_as::<Proto>(),
            signatures: [signature].to_vec(),
        }
        .encode_as::<Proto>();

        let tx_hash: H256 = sha2::Sha256::new()
            .chain_update(&tx_raw_bytes)
            .finalize()
            .into();

        if let Ok(tx) = self.client.tx(tx_hash, false).await {
            debug!(%tx_hash, "tx already included");
            return Ok((tx_hash, tx));
        }

        let response = self
            .client
            .broadcast_tx_sync(&tx_raw_bytes)
            .await
            .context("broadcast_tx_sync")?;

        assert_eq!(tx_hash, response.hash, "tx hash calculated incorrectly");

        info!(%tx_hash);

        info!(
            check_tx_code = %response.code,
            codespace = %response.codespace,
            check_tx_log = %response.log
        );

        if response.code > 0 {
            bail!(
                "cosmos tx failed: {}, {}: {}",
                response.code,
                response.codespace,
                response.log
            );
        };

        let mut target_height = self
            .client
            .block(None)
            .await
            .context("querying latest block")?
            .block
            .header
            .height;

        let mut i = 0;
        loop {
            let reached_height = 'l: loop {
                let current_height = self
                    .client
                    .block(None)
                    .await
                    .context("querying latest block for tx inclusion")?
                    .block
                    .header
                    .height;

                if current_height >= target_height {
                    break 'l current_height;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            };

            let tx_inclusion = self.client.tx(tx_hash, false).await;

            // debug!(?tx_inclusion);

            match tx_inclusion {
                Ok(tx) => {
                    if tx.tx_result.code == 0 {
                        break Ok((tx_hash, tx));
                    } else {
                        bail!(
                            "cosmos tx failed: {}, {}: {}",
                            response.code,
                            response.codespace,
                            response.log
                        );
                    }
                }
                Err(err) if i > 5 => {
                    return Err(anyhow!(
                        "tx inclusion couldn't be retrieved after {i} attempt(s) (tx hash: {tx_hash})"
                    )
                    .context(err));
                }
                Err(_) => {
                    debug!("unable to retrieve tx inclusion, trying again");
                    target_height = reached_height.add(&1);
                    i += 1;
                    continue;
                }
            }
        }
    }

    pub async fn simulate_tx(
        &self,
        messages: impl IntoIterator<Item = protos::google::protobuf::Any> + Clone,
    ) -> Result<(TxBody, AuthInfo, GasInfo)> {
        use protos::cosmos::tx;

        let account = self
            .account_info(&self.signer.to_string())
            .await
            .context("querying account info")?;

        let tx_body = TxBody {
            // TODO: Use RawAny here
            messages: messages.clone().into_iter().map(Into::into).collect(),
            memo: String::new(),
            timeout_height: 0,
            extension_options: vec![],
            non_critical_extension_options: vec![],
            unordered: false,
            timeout_timestamp: None,
        };

        let auth_info = AuthInfo {
            signer_infos: [SignerInfo {
                public_key: Some(AnyPubKey::Secp256k1(secp256k1::PubKey {
                    key: self.signer.public_key().into(),
                })),
                mode_info: ModeInfo::Single {
                    mode: SignMode::Direct,
                },
                sequence: account.sequence,
            }]
            .to_vec(),
            fee: self.gas_config.mk_fee(self.gas_config.max_gas).clone(),
        };

        let simulation_signature = self
            .signer
            .try_sign(
                &SignDoc {
                    body_bytes: tx_body.clone().encode_as::<Proto>(),
                    auth_info_bytes: auth_info.clone().encode_as::<Proto>(),
                    chain_id: self.chain_id.to_string(),
                    account_number: account.account_number,
                }
                .encode_as::<Proto>(),
            )
            .expect("signing failed")
            .to_bytes()
            .to_vec();

        let simulate_response = self
            .client
            .grpc_abci_query::<_, tx::v1beta1::SimulateResponse>(
                "/cosmos.tx.v1beta1.Service/Simulate",
                &tx::v1beta1::SimulateRequest {
                    tx_bytes: Tx {
                        body: tx_body.clone(),
                        auth_info: auth_info.clone(),
                        signatures: [simulation_signature.clone()].to_vec(),
                    }
                    .encode_as::<Proto>(),
                    ..Default::default()
                },
                None,
                false,
            )
            .await
            .context("submitting SimulateRequest")?
            .into_result()?;

        let result = simulate_response.unwrap();

        Ok((
            tx_body,
            auth_info,
            result
                .gas_info
                .expect("gas info is present on successful simulation result")
                .into(),
        ))
    }

    async fn account_info(&self, account: &str) -> Result<BaseAccount> {
        debug!(%account, "fetching account");

        Ok(self
            .client
            .grpc_abci_query::<_, protos::cosmos::auth::v1beta1::QueryAccountResponse>(
                "/cosmos.auth.v1beta1.Query/Account",
                &protos::cosmos::auth::v1beta1::QueryAccountRequest {
                    address: account.to_string(),
                },
                None,
                false,
            )
            .await
            .context("querying account info")?
            .into_result()?
            .unwrap()
            .account
            .map(<Any<BaseAccount>>::try_from)
            .context("decoding account info")??
            .0)
    }
}

fn instantiate2_address(address: Bech32<Bytes>, checksum: H256, salt: &str) -> Result<String> {
    let addr = cosmwasm_std::instantiate2_address(
        checksum.get(),
        &address.data()[..].into(),
        salt.as_bytes(),
    )?;

    Ok(Bech32::new(address.hrp(), &*addr).to_string())
}
