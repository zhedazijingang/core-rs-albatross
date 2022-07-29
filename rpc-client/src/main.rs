use anyhow::{bail, Error};
use clap::{Args, Parser};
use futures::StreamExt;
use url::Url;

use nimiq_jsonrpc_client::{websocket::WebsocketClient, ArcClient};
use nimiq_jsonrpc_core::Credentials;
use nimiq_keys::Address;
use nimiq_primitives::coin::Coin;
use nimiq_rpc_interface::{
    blockchain::{BlockchainInterface, BlockchainProxy},
    consensus::{ConsensusInterface, ConsensusProxy},
    mempool::MempoolProxy,
    types::{BlockNumberOrHash, HashAlgorithm, LogType, ValidityStartHeight},
    validator::{ValidatorInterface, ValidatorProxy},
    wallet::{WalletInterface, WalletProxy},
};
use nimiq_transaction::account::htlc_contract::AnyHash;

#[derive(Debug, Parser)]
struct Opt {
    #[clap(short)]
    url: Option<String>,

    #[clap(short = 'U')]
    username: Option<String>,

    #[clap(short = 'P')]
    password: Option<String>,

    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
enum Command {
    /// Query a block from the blockchain.
    Block {
        /// Either a block hash or number. If omitted, the last block is queried.
        hash_or_number: Option<BlockNumberOrHash>,

        /// Include transactions
        #[clap(short = 't')]
        include_transactions: bool,
    },

    /// Lists the current stakes from the staking contract.
    Stakes {},

    /// Follow the head of the blockchain.
    FollowHead {
        /// Show the full block instead of only the hash.
        #[clap(short)]
        block: bool,
    },
    /// Follow a validator state upon election blocks.
    FollowValidator { address: Address },

    /// Follow the logs associated with the specified addresses and of any of the log types given.
    /// If no addresses or no logtypes are provided it fetches all logs.
    FollowLogsOfAddressesAndTypes {
        #[clap(short = 'a', long, multiple_values = true)]
        addresses: Vec<Address>,

        /// Possible values are:
        #[clap(short = 'l', long, arg_enum, multiple_values = true)]
        log_types: Vec<LogType>,
    },

    /// Show wallet accounts and their balances.
    #[clap(flatten)]
    Account(AccountCommand),

    /// Create, sign and send transactions.
    #[clap(name = "tx", flatten)]
    Transaction(TransactionCommand),

    /// Changes the automatic reactivation setting for the current validator.
    SetAutoReactivateValidator {
        /// The validator setting for automatic reactivation to be applied.
        #[clap(short, long)]
        automatic_reactivate: bool,
    },
}

#[derive(Debug, Parser)]
enum AccountCommand {
    List {
        #[clap(short, long)]
        short: bool,
    },

    New {
        #[clap(short = 'P', long)]
        password: Option<String>,
    },

    Import {
        #[clap(short = 'P', long)]
        password: Option<String>,

        key_data: String,
    },

    Lock {
        address: Address,
    },

    Unlock {
        #[clap(short = 'P', long)]
        password: Option<String>,

        address: Address,
    },
    /// Queries the account state (e.g. account balance for basic accounts).
    Get {
        address: Address,
    },
}

#[derive(Debug, Args)]
pub struct TxCommounWithValue {
    /// The amount of NIM to be used by the transaction.
    value: Coin,

    #[clap(flatten)]
    commoun_tx_fields: TxCommoun,
}

#[derive(Debug, Args)]
struct TxCommoun {
    /// The associated transaction fee to be payed. If absent it defaults to 0 NIM.
    #[clap(short, long, default_value = "0")]
    fee: Coin,

    /// The block height from which on the transaction could be applied. The maximum amount of blocks the transaction is valid for
    /// is specified in `TRANSACTION_VALIDITY_WINDOW`.
    /// If absent it defaults to the current block height at time of processing.
    #[clap(short, long, default_value_t)]
    validity_start_height: ValidityStartHeight,

    /// Don't actually send the transaction, but output the transaction as hex string.
    #[clap(long = "dry")]
    dry: bool,
}

#[derive(Debug, Parser)]
enum TransactionCommand {
    /// Sends a simple transaction from the wallet `wallet` to a basic `recipient`.
    Basic {
        /// Transaction will be sent from this address. An wallet with this address must be unlocked.
        sender_wallet: Address,

        /// Recipient for this transaction. This must be a basic account.
        recipient: Address,

        #[clap(flatten)]
        tx_commons: TxCommounWithValue,
    },

    /** Staker transactions **/

    /// Sends a `new_staker` transaction to the network. You need to provide the address of a basic
    /// account (the sender wallet) to pay the transaction fee.
    NewStaker {
        /// The stake will be sent from this wallet.
        sender_wallet: Address,

        /// Destination address for the stake.
        staker_address: Address,

        /// Validator address to delegate stake to. If empty, no delegation will occour.
        delegation: Option<Address>,

        #[clap(flatten)]
        tx_commons: TxCommounWithValue,
    },

    /// Sends a staking transaction from the address of a given `wallet` to a given `staker_address`.
    Stake {
        /// The stake will be sent from this wallet.
        sender_wallet: Address,

        /// Destination address for the stake.
        staker_address: Address,

        #[clap(flatten)]
        tx_commons: TxCommounWithValue,
    },

    /// Sends a `update_staker` transaction to the network. You can pay the transaction fee from a basic
    /// account (by providing the sender wallet) or from the staker account's balance (by not
    /// providing a sender wallet).
    UpdateStaker {
        /// The fee will be payed by this wallet if any is provided. If absent the fee is payed by the stakers account.
        sender_wallet: Option<Address>,

        /// Destination address for the update.
        staker_address: Address,

        /// The new address for the delegation. If none is provided the delegation will remain the same. ??
        new_delegation: Option<Address>,

        #[clap(flatten)]
        tx_commons: TxCommoun,
    },

    /// Sends a `unstake` transaction to the network. The transaction fee will be paid from the funds
    /// being unstaked.
    Unstake {
        /// The stake will be sent from this wallet.
        sender_wallet: Address,

        /// The recipients of the previously staked coins.
        recipient: Address,

        /// The amount of NIM to unstake.
        #[clap(flatten)]
        tx_commons: TxCommounWithValue,
    },

    /** Validator transactions **/

    /// Sends a `new_validator` transaction to the network. You need to provide the address of a basic
    /// account (the sender wallet) to pay the transaction fee and the validator deposit.
    /// Since JSON doesn't have a primitive for Option (it just has the null primitive), we can't
    /// have a double Option. So we use the following work-around for the signal data:
    ///  "" = Set the signal data field to None.
    ///  "0x29a4b..." = Set the signal data field to Some(0x29a4b...).
    CreateValidator {
        /// The fee will be payed from this wallet.
        sender_wallet: Address,

        // The new validator address.
        validator_address: Address,

        // The secret key for validator to sign its transactions. ??
        signing_secret_key: String,

        // The secret key for the validator to sign the voting transactions. ??
        voting_secret_key: String,

        // The address to receive the rewards.
        reward_address: Address,

        // ??
        signal_data: String,

        #[clap(flatten)]
        tx_commons: TxCommoun,
    },

    /// Sends a transaction to the network to update this validator. You need to provide the address of a basic
    /// account (the sender wallet) to pay the transaction fee.
    ///  Since JSON doesn't have a primitive for Option (it just has the null primitive), we can't
    /// have a double Option. So we use the following work-around for the signal data:
    ///  null = No change in the signal data field.
    ///  "" = Change the signal data field to None.
    ///  "0x29a4b..." = Change the signal data field to Some(0x29a4b...).
    UpdateValidator {
        /// The fee will be payed from this wallet.
        sender_wallet: Address,

        // The secret key for validator to sign its transactions. ??
        new_signing_secret_key: Option<String>,

        // The secret key for the validator to sign the voting transactions. ??
        new_voting_secret_key: Option<String>,

        // The address to receive the rewards.
        new_reward_address: Option<Address>,

        // ??
        new_signal_data: Option<String>,

        #[clap(flatten)]
        tx_commons: TxCommoun,
    },

    /// Sends a transaction to inactivate this validator. In order to avoid having the validator reactivated soon after
    /// this transacation takes effect, use the command set-auto-reactivate-validator to make sure the automatic reactivation
    /// configuration is turned off.
    InactivateValidator {
        /// The fee will be payed from this wallet.
        sender_wallet: Address,

        #[clap(flatten)]
        tx_commons: TxCommoun,
    },

    /// Sends a transaction to reactivate this validator. You need to provide the address of a basic
    /// account (the sender wallet) to pay the transaction fee.
    ReactivateValidator {
        /// The fee will be payed from this wallet.
        sender_wallet: Address,

        #[clap(flatten)]
        tx_commons: TxCommoun,
    },

    /// Sends a transaction to unpark this validator. You need to provide the address of a basic
    /// account (the sender wallet) to pay the transaction fee.
    UnparkValidator {
        /// The fee will be payed from this wallet.
        sender_wallet: Address,

        #[clap(flatten)]
        tx_commons: TxCommoun,
    },

    /// Sends a transaction to delete this validator. The transaction fee will be paid from the
    /// validator deposit that is being returned.
    DeleteValidator {
        /// The address to receive the balance of the validator.
        recipient_address: Address,

        #[clap(flatten)]
        tx_commons: TxCommoun,
    },

    /** Vesting transactions **/
    VestingCreate {
        /// The wallet used to sign the transaction. The vesting contract value is sent from the basic account
        /// belonging to this wallet.
        sender_wallet: Address,

        /// The owner of the vesting contract.
        owner: Address,

        start_time: u64,

        time_step: u64,

        /// Create a release schedule of `num_steps` payouts of value starting at `start_time + time_step`.
        num_steps: u32,

        #[clap(flatten)]
        tx_commons: TxCommounWithValue,
    },

    VestingRedeem {
        /// The wallet to sign the transaction. This wallet should be the owner of the vesting contract
        sender_wallet: Address,

        /// The vesting contract address.
        contract_address: Address,

        /// The address of the basic account that will receive the funds.
        recipient: Address,

        #[clap(flatten)]
        tx_commons: TxCommounWithValue,
    },

    /** HTLC transactions **/
    CreateHTLC {
        /// The wallet to sign the transaction. The HTLC contract value is sent from the basic account belonging to this wallet.
        sender_wallet: Address,

        /// The address of the sender in the HTLC contract.
        htlc_sender: Address,

        /// The address of the recipient in the HTLC contract.
        htlc_recipient: Address,

        #[clap(short = 'r', long)]
        hash_root: AnyHash,

        #[clap(short = 'c', long = "count")]
        hash_count: u8,

        /// The `hash_root` is the result of hashing the pre-image `hash_count` times using `hash_algorithm`.
        #[clap(short = 'a', long, arg_enum)]
        hash_algorithm: HashAlgorithm,

        /// Sets the blockchain height at which the `htlc_sender` automatically gains control over the funds.
        timeout: u64,

        #[clap(flatten)]
        tx_commons: TxCommounWithValue,
    },

    RedeemRegularHTLC {
        /// This address corresponds to the `htlc_recipient` in the HTLC contract.
        sender_wallet: Address,

        /// The address of the HTLC contract.
        contract_address: Address,

        /// The address of the basic account that will receive the funds.
        htlc_recipient: Address,

        pre_image: AnyHash,

        #[clap(short = 'r', long)]
        hash_root: AnyHash,

        #[clap(short = 'c', long)]
        hash_count: u8,

        /// The `hash_root` is the result of hashing the `pre_image` `hash_count` times using `hash_algorithm`.
        #[clap(short = 'a', long, arg_enum)]
        hash_algorithm: HashAlgorithm,

        #[clap(flatten)]
        tx_commons: TxCommounWithValue,
    },

    RedeemHTLCTimeout {
        /// This address corresponds to the `htlc_recipient` in the HTLC contract.
        sender_wallet: Address,

        /// The address of the HTLC contract.
        contract_address: Address,

        /// The address of the basic account that will receive the funds.
        htlc_recipient: Address,

        #[clap(flatten)]
        tx_commons: TxCommounWithValue,
    },

    RedeemHTLCEarly {
        /// The address of the HTLC contract.
        contract_address: Address,

        /// The address of the basic account that will receive the funds.
        htlc_recipient: Address,

        /// The signature corresponding to the `htlc_sender` in the HTLC contract.
        htlc_sender_signature: String,

        /// The signature corresponding to the `htlc_recipient` in the HTLC contract.
        htlc_recipient_signature: String,

        #[clap(flatten)]
        tx_commons: TxCommounWithValue,
    },

    SignRedeemHTLCEarly {
        /// This is the address used to sign the transaction. It corresponds either to the `htlc_sender` or the `htlc_recipient`
        /// in the HTLC contract.
        sender_wallet: Address,

        /// The address of the HTLC contract.
        contract_address: Address,

        /// The address of the basic account that will receive the funds.
        htlc_recipient: Address,

        #[clap(flatten)]
        tx_commons: TxCommounWithValue,
    },
}

impl Command {
    async fn run(self, mut client: Client) -> Result<(), Error> {
        match self {
            Command::Block {
                hash_or_number,
                include_transactions,
            } => {
                let block = match hash_or_number {
                    Some(BlockNumberOrHash::Hash(hash)) => {
                        client
                            .blockchain
                            .get_block_by_hash(hash, Some(include_transactions))
                            .await
                    }
                    Some(BlockNumberOrHash::Number(number)) => {
                        client
                            .blockchain
                            .get_block_by_number(number, Some(include_transactions))
                            .await
                    }
                    None => {
                        client
                            .blockchain
                            .get_latest_block(Some(include_transactions))
                            .await
                    }
                }?;

                println!("{:#?}", block)
            }

            Command::Stakes {} => {
                let stakes = client.blockchain.get_active_validators().await?;
                println!("{:#?}", stakes);
            }

            Command::FollowHead { block: show_block } => {
                if show_block {
                    let mut stream = client
                        .blockchain
                        .subscribe_for_head_block(Some(false))
                        .await?;

                    while let Some(block) = stream.next().await {
                        println!("{:#?}", block);
                    }
                } else {
                    let mut stream = client.blockchain.subscribe_for_head_block_hash().await?;

                    while let Some(block_hash) = stream.next().await {
                        println!("{}", block_hash);
                    }
                }
            }

            Command::FollowValidator { address } => {
                let mut stream = client
                    .blockchain
                    .subscribe_for_validator_election_by_address(address)
                    .await?;
                while let Some(validator) = stream.next().await {
                    println!("{:#?}", validator);
                }
            }

            Command::FollowLogsOfAddressesAndTypes {
                addresses,
                log_types,
            } => {
                let mut stream = client
                    .blockchain
                    .subscribe_for_logs_by_addresses_and_types(addresses, log_types)
                    .await?;

                while let Some(blocklog) = stream.next().await {
                    println!("{:#?}", blocklog);
                }
            }
            Command::SetAutoReactivateValidator {
                automatic_reactivate,
            } => {
                let result = client
                    .validator
                    .set_automatic_reactivation(automatic_reactivate)
                    .await?;
                println!("Auto reacivate set to {}", result);
            }

            Command::Account(command) => {
                match command {
                    AccountCommand::List { short } => {
                        let accounts = client.wallet.list_accounts().await?;
                        for address in &accounts {
                            if short {
                                println!("{}", address.to_user_friendly_address());
                            } else {
                                let account = client
                                    .blockchain
                                    .get_account_by_address(address.clone())
                                    .await?;
                                println!("{}: {:#?}", address.to_user_friendly_address(), account);
                            }
                        }
                    }

                    AccountCommand::New { password } => {
                        let account = client.wallet.create_account(password).await?;
                        println!("{:#?}", account);
                    }

                    AccountCommand::Import { password, key_data } => {
                        let address = client.wallet.import_raw_key(key_data, password).await?;
                        println!("{}", address);
                    }

                    AccountCommand::Lock { address } => {
                        client.wallet.lock_account(address).await?;
                    }

                    AccountCommand::Unlock {
                        address, password, ..
                    } => {
                        // TODO: Duration
                        client
                            .wallet
                            .unlock_account(address, password, None)
                            .await?;
                    }

                    AccountCommand::Get { address } => {
                        let account = client.blockchain.get_account_by_address(address).await?;
                        println!("{:#?}", account);
                    }
                }
            }

            Command::Transaction(command) => match command {
                TransactionCommand::Basic {
                    sender_wallet,
                    recipient,
                    tx_commons,
                } => {
                    if tx_commons.commoun_tx_fields.dry {
                        let tx = client
                            .consensus
                            .create_basic_transaction(
                                sender_wallet,
                                recipient,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_basic_transaction(
                                sender_wallet,
                                recipient,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }
                TransactionCommand::NewStaker {
                    sender_wallet,
                    staker_address,
                    delegation,
                    tx_commons,
                } => {
                    if tx_commons.commoun_tx_fields.dry {
                        let tx = client
                            .consensus
                            .create_new_staker_transaction(
                                sender_wallet,
                                staker_address,
                                delegation,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_new_staker_transaction(
                                sender_wallet,
                                staker_address,
                                delegation,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }

                TransactionCommand::Stake {
                    sender_wallet,
                    staker_address,
                    tx_commons,
                } => {
                    if tx_commons.commoun_tx_fields.dry {
                        let tx = client
                            .consensus
                            .create_stake_transaction(
                                sender_wallet,
                                staker_address,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_stake_transaction(
                                sender_wallet,
                                staker_address,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }

                TransactionCommand::UpdateStaker {
                    sender_wallet,
                    staker_address,
                    new_delegation,
                    tx_commons,
                } => {
                    if tx_commons.dry {
                        let tx = client
                            .consensus
                            .create_update_transaction(
                                sender_wallet,
                                staker_address,
                                new_delegation,
                                tx_commons.fee,
                                tx_commons.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_update_transaction(
                                sender_wallet,
                                staker_address,
                                new_delegation,
                                tx_commons.fee,
                                tx_commons.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }

                TransactionCommand::Unstake {
                    sender_wallet,
                    recipient,
                    tx_commons,
                } => {
                    if tx_commons.commoun_tx_fields.dry {
                        let tx = client
                            .consensus
                            .create_unstake_transaction(
                                sender_wallet,
                                recipient,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_unstake_transaction(
                                sender_wallet,
                                recipient,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }

                TransactionCommand::CreateValidator {
                    sender_wallet,
                    validator_address,
                    signing_secret_key,
                    voting_secret_key,
                    reward_address,
                    signal_data,
                    tx_commons,
                } => {
                    if tx_commons.dry {
                        let tx = client
                            .consensus
                            .create_new_validator_transaction(
                                sender_wallet,
                                validator_address,
                                signing_secret_key,
                                voting_secret_key,
                                reward_address,
                                signal_data,
                                tx_commons.fee,
                                tx_commons.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_new_validator_transaction(
                                sender_wallet,
                                validator_address,
                                signing_secret_key,
                                voting_secret_key,
                                reward_address,
                                signal_data,
                                tx_commons.fee,
                                tx_commons.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }

                TransactionCommand::UpdateValidator {
                    sender_wallet,
                    new_signing_secret_key,
                    new_voting_secret_key,
                    new_reward_address,
                    new_signal_data,
                    tx_commons,
                } => {
                    let validator_address = client.validator.get_address().await?;
                    if tx_commons.dry {
                        let tx = client
                            .consensus
                            .create_update_validator_transaction(
                                sender_wallet,
                                validator_address,
                                new_signing_secret_key,
                                new_voting_secret_key,
                                new_reward_address,
                                new_signal_data,
                                tx_commons.fee,
                                tx_commons.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_update_validator_transaction(
                                sender_wallet,
                                validator_address,
                                new_signing_secret_key,
                                new_voting_secret_key,
                                new_reward_address,
                                new_signal_data,
                                tx_commons.fee,
                                tx_commons.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }

                TransactionCommand::InactivateValidator {
                    sender_wallet,
                    tx_commons,
                } => {
                    let validator_address = client.validator.get_address().await?;
                    let key_data = client.validator.get_signing_key().await?;
                    if tx_commons.dry {
                        let tx = client
                            .consensus
                            .create_inactivate_validator_transaction(
                                sender_wallet,
                                validator_address,
                                key_data,
                                tx_commons.fee,
                                tx_commons.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_inactivate_validator_transaction(
                                sender_wallet,
                                validator_address,
                                key_data,
                                tx_commons.fee,
                                tx_commons.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }

                TransactionCommand::ReactivateValidator {
                    sender_wallet,
                    tx_commons,
                } => {
                    let validator_address = client.validator.get_address().await?;
                    let key_data = client.validator.get_signing_key().await?;
                    if tx_commons.dry {
                        let tx = client
                            .consensus
                            .create_reactivate_validator_transaction(
                                sender_wallet,
                                validator_address,
                                key_data,
                                tx_commons.fee,
                                tx_commons.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_reactivate_validator_transaction(
                                sender_wallet,
                                validator_address,
                                key_data,
                                tx_commons.fee,
                                tx_commons.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }

                TransactionCommand::UnparkValidator {
                    sender_wallet,
                    tx_commons,
                } => {
                    let validator_address = client.validator.get_address().await?;
                    let key_data = client.validator.get_signing_key().await?;
                    if tx_commons.dry {
                        let tx = client
                            .consensus
                            .create_unpark_validator_transaction(
                                sender_wallet,
                                validator_address,
                                key_data,
                                tx_commons.fee,
                                tx_commons.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_unpark_validator_transaction(
                                sender_wallet,
                                validator_address,
                                key_data,
                                tx_commons.fee,
                                tx_commons.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }

                TransactionCommand::DeleteValidator {
                    recipient_address,
                    tx_commons,
                } => {
                    let validator_address = client.validator.get_address().await?;
                    if tx_commons.dry {
                        let tx = client
                            .consensus
                            .create_delete_validator_transaction(
                                validator_address,
                                recipient_address,
                                tx_commons.fee,
                                tx_commons.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_delete_validator_transaction(
                                validator_address,
                                recipient_address,
                                tx_commons.fee,
                                tx_commons.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }

                TransactionCommand::VestingCreate {
                    sender_wallet,
                    owner,
                    start_time,
                    time_step,
                    num_steps,
                    tx_commons,
                } => {
                    if tx_commons.commoun_tx_fields.dry {
                        let tx = client
                            .consensus
                            .create_new_vesting_transaction(
                                sender_wallet,
                                owner,
                                start_time,
                                time_step,
                                num_steps,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_new_vesting_transaction(
                                sender_wallet,
                                owner,
                                start_time,
                                time_step,
                                num_steps,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }
                TransactionCommand::VestingRedeem {
                    sender_wallet,
                    contract_address,
                    recipient,
                    tx_commons,
                } => {
                    if tx_commons.commoun_tx_fields.dry {
                        let tx = client
                            .consensus
                            .create_redeem_vesting_transaction(
                                sender_wallet,
                                contract_address,
                                recipient,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_redeem_vesting_transaction(
                                sender_wallet,
                                contract_address,
                                recipient,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }
                TransactionCommand::CreateHTLC {
                    sender_wallet,
                    htlc_sender,
                    htlc_recipient,
                    hash_root,
                    hash_count,
                    hash_algorithm,
                    timeout,
                    tx_commons,
                } => {
                    if tx_commons.commoun_tx_fields.dry {
                        let tx = client
                            .consensus
                            .create_new_htlc_transaction(
                                sender_wallet,
                                htlc_sender,
                                htlc_recipient,
                                hash_root,
                                hash_count,
                                hash_algorithm.into(),
                                timeout,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_new_htlc_transaction(
                                sender_wallet,
                                htlc_sender,
                                htlc_recipient,
                                hash_root,
                                hash_count,
                                hash_algorithm.into(),
                                timeout,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }
                TransactionCommand::RedeemRegularHTLC {
                    sender_wallet,
                    contract_address,
                    htlc_recipient,
                    pre_image,
                    hash_root,
                    hash_count,
                    hash_algorithm,
                    tx_commons,
                } => {
                    if tx_commons.commoun_tx_fields.dry {
                        let tx = client
                            .consensus
                            .create_redeem_regular_htlc_transaction(
                                sender_wallet,
                                contract_address,
                                htlc_recipient,
                                pre_image,
                                hash_root,
                                hash_count,
                                hash_algorithm.into(),
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_redeem_regular_htlc_transaction(
                                sender_wallet,
                                contract_address,
                                htlc_recipient,
                                pre_image,
                                hash_root,
                                hash_count,
                                hash_algorithm.into(),
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }
                TransactionCommand::RedeemHTLCTimeout {
                    sender_wallet,
                    contract_address,
                    htlc_recipient,
                    tx_commons,
                } => {
                    if tx_commons.commoun_tx_fields.dry {
                        let tx = client
                            .consensus
                            .create_redeem_timeout_htlc_transaction(
                                sender_wallet,
                                contract_address,
                                htlc_recipient,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_redeem_timeout_htlc_transaction(
                                sender_wallet,
                                contract_address,
                                htlc_recipient,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }
                TransactionCommand::RedeemHTLCEarly {
                    contract_address,
                    htlc_recipient,
                    htlc_sender_signature,
                    htlc_recipient_signature,

                    tx_commons,
                } => {
                    if tx_commons.commoun_tx_fields.dry {
                        let tx = client
                            .consensus
                            .create_redeem_early_htlc_transaction(
                                contract_address,
                                htlc_recipient,
                                htlc_sender_signature,
                                htlc_recipient_signature,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", tx);
                    } else {
                        let txid = client
                            .consensus
                            .send_redeem_early_htlc_transaction(
                                contract_address,
                                htlc_recipient,
                                htlc_sender_signature,
                                htlc_recipient_signature,
                                tx_commons.value,
                                tx_commons.commoun_tx_fields.fee,
                                tx_commons.commoun_tx_fields.validity_start_height,
                            )
                            .await?;
                        println!("{}", txid);
                    }
                }
                TransactionCommand::SignRedeemHTLCEarly {
                    sender_wallet,
                    contract_address,
                    htlc_recipient,
                    tx_commons,
                } => {
                    let tx = client
                        .consensus
                        .sign_redeem_early_htlc_transaction(
                            sender_wallet,
                            contract_address,
                            htlc_recipient,
                            tx_commons.value,
                            tx_commons.commoun_tx_fields.fee,
                            tx_commons.commoun_tx_fields.validity_start_height,
                        )
                        .await?;
                    println!("{}", tx);
                }
            },
        }

        Ok(())
    }
}

pub struct Client {
    pub blockchain: BlockchainProxy<ArcClient<WebsocketClient>>,
    pub consensus: ConsensusProxy<ArcClient<WebsocketClient>>,
    pub mempool: MempoolProxy<ArcClient<WebsocketClient>>,
    pub wallet: WalletProxy<ArcClient<WebsocketClient>>,
    pub validator: ValidatorProxy<ArcClient<WebsocketClient>>,
}

impl Client {
    pub async fn new(url: Url, credentials: Option<Credentials>) -> Result<Self, Error> {
        let client = ArcClient::new(WebsocketClient::new(url, credentials).await?);

        Ok(Self {
            blockchain: BlockchainProxy::new(client.clone()),
            consensus: ConsensusProxy::new(client.clone()),
            mempool: MempoolProxy::new(client.clone()),
            wallet: WalletProxy::new(client.clone()),
            validator: ValidatorProxy::new(client),
        })
    }
}

async fn run_app(opt: Opt) -> Result<(), Error> {
    let url = opt
        .url
        .as_deref()
        .unwrap_or("ws://127.0.0.1:8648/ws")
        .parse()?;

    let credentials = match (&opt.username, &opt.password) {
        (Some(username), Some(password)) => Some(Credentials {
            username: username.to_string(),
            password: password.to_string(),
        }),
        (None, None) => None,
        _ => bail!("Both username and password needs to be specified."),
    };

    let client = Client::new(url, credentials).await?;

    opt.command.run(client).await?;

    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(e) = dotenv::dotenv() {
        if !e.not_found() {
            panic!("could not read .env file: {}", e);
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    if let Err(e) = run_app(Opt::parse()).await {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
