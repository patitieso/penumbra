use anyhow::{anyhow, Context};
use camino::Utf8Path;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use penumbra_chain::params::{ChainParameters, FmdParameters};
use penumbra_crypto::{
    asset::{self, Id},
    dex::{
        lp::position::{self, State},
        TradingPair,
    },
    note,
    stake::{DelegationToken, IdentityKey},
    Address, Amount, Asset, FieldExt, Fq, FullViewingKey, Note, Nullifier, Rseed, Value,
};
use penumbra_proto::{
    client::v1alpha1::{
        oblivious_query_service_client::ObliviousQueryServiceClient, ChainParametersRequest,
    },
    DomainType,
};
use penumbra_tct as tct;
use penumbra_transaction::Transaction;
use r2d2_sqlite::{
    rusqlite::{OpenFlags, OptionalExtension},
    SqliteConnectionManager,
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, num::NonZeroU64, str::FromStr, sync::Arc, time::Duration};
use tct::Commitment;
use tokio::{
    sync::broadcast::{self, error::RecvError},
    task::spawn_blocking,
};
use url::Url;

use crate::{sync::FilteredBlock, SpendableNoteRecord, SwapRecord};

mod sct;
use sct::TreeStore;

/// The hash of the schema for the database.
static SCHEMA_HASH: Lazy<String> =
    Lazy::new(|| hex::encode(Sha256::digest(include_str!("storage/schema.sql"))));

#[derive(Clone)]
pub struct Storage {
    pool: r2d2::Pool<SqliteConnectionManager>,

    /// This allows an optimization where we only commit to the database after
    /// scanning a nonempty block.
    ///
    /// If this is `Some`, we have uncommitted empty blocks up to the inner height.
    /// If this is `None`, we don't.
    ///
    /// Using a `NonZeroU64` ensures that `Option<NonZeroU64>` fits in 8 bytes.
    uncommitted_height: Arc<Mutex<Option<NonZeroU64>>>,

    scanned_notes_tx: tokio::sync::broadcast::Sender<SpendableNoteRecord>,
    scanned_nullifiers_tx: tokio::sync::broadcast::Sender<Nullifier>,
    scanned_swaps_tx: tokio::sync::broadcast::Sender<SwapRecord>,
}

impl Storage {
    /// If the database at `storage_path` exists, [`Self::load`] it, otherwise, [`Self::initialize`] it.
    pub async fn load_or_initialize(
        storage_path: Option<impl AsRef<Utf8Path>>,
        fvk: &FullViewingKey,
        node: Url,
    ) -> anyhow::Result<Self> {
        if let Some(path) = storage_path.as_ref() {
            if path.as_ref().exists() {
                return Self::load(
                    storage_path.expect(
                        "storage path is not `None` because we already matched on it above",
                    ),
                )
                .await;
            }
        };

        let mut client = ObliviousQueryServiceClient::connect(node.to_string()).await?;
        let params = client
            .chain_parameters(tonic::Request::new(ChainParametersRequest {
                chain_id: String::new(),
            }))
            .await?
            .into_inner()
            .try_into()?;

        Self::initialize(storage_path, fvk.clone(), params).await
    }

    fn connect(
        path: Option<impl AsRef<Utf8Path>>,
    ) -> anyhow::Result<r2d2::Pool<SqliteConnectionManager>> {
        if let Some(path) = path {
            let manager = SqliteConnectionManager::file(path.as_ref())
                .with_flags(
                    // Don't allow opening URIs, because they can change the behavior of the database; we
                    // just want to open normal filepaths.
                    OpenFlags::default() & !OpenFlags::SQLITE_OPEN_URI,
                )
                .with_init(|conn| {
                    // We use `prepare_cached` a fair amount: this is an overestimate of the number
                    // of cached prepared statements likely to be used.
                    conn.set_prepared_statement_cache_capacity(32);
                    Ok(())
                });
            Ok(r2d2::Pool::new(manager)?)
        } else {
            let manager = SqliteConnectionManager::memory();
            // Max size needs to be set to 1, otherwise a new in-memory database is created for each
            // connection to the pool, which results in very confusing errors.
            //
            // Lifetimes and timeouts are likewise configured to their maximum values, since
            // the in-memory database will disappear on connection close.
            Ok(r2d2::Pool::builder()
                .max_size(1)
                .min_idle(Some(1))
                .max_lifetime(Some(Duration::MAX))
                .idle_timeout(Some(Duration::MAX))
                .build(manager)?)
        }
    }

    pub async fn load(path: impl AsRef<Utf8Path>) -> anyhow::Result<Self> {
        let storage = Self {
            pool: Self::connect(Some(path))?,
            uncommitted_height: Arc::new(Mutex::new(None)),
            scanned_notes_tx: broadcast::channel(128).0,
            scanned_nullifiers_tx: broadcast::channel(512).0,
            scanned_swaps_tx: broadcast::channel(128).0,
        };

        spawn_blocking(move || {
            // Check the version of the software used when first initializing this database.
            // If it doesn't match the current version, we should report the error to the user.
            let actual_schema_hash: String = storage
                .pool
                .get()?
                .query_row("SELECT schema_hash FROM schema_hash", (), |row| {
                    row.get("schema_hash")
                })
                .context("failed to query database schema version: the database was probably created by an old client version, and needs to be reset and resynchronized")?;

            if actual_schema_hash != *SCHEMA_HASH {
                let database_client_version: String = storage
                    .pool
                    .get()?
                    .query_row("SELECT client_version FROM client_version", (), |row| {
                        row.get("client_version")
                    })
                    .context("failed to query client version: the database was probably created by an old client version, and needs to be reset and resynchronized")?;

                return Err(anyhow!(
                    "can't load view database created by client version {} using client version {}: they have different schemata, so you need to reset your view database and resynchronize",
                    database_client_version,
                    env!("VERGEN_GIT_SEMVER"),
                ));
            }

            Ok(storage)
        })
        .await?
    }

    pub async fn initialize(
        storage_path: Option<impl AsRef<Utf8Path>>,
        fvk: FullViewingKey,
        params: ChainParameters,
    ) -> anyhow::Result<Self> {
        tracing::debug!(storage_path = ?storage_path.as_ref().map(AsRef::as_ref), ?fvk, ?params);

        // Connect to the database (or create it)
        let pool = Self::connect(storage_path)?;

        spawn_blocking(move || {
            // In one database transaction, populate everything
            let mut conn = pool.get()?;
            let tx = conn.transaction()?;

            // Create the tables
            tx.execute_batch(include_str!("storage/schema.sql"))?;

            let chain_params_bytes = &ChainParameters::encode_to_vec(&params)[..];
            tx.execute(
                "INSERT INTO chain_params (bytes) VALUES (?1)",
                [chain_params_bytes],
            )?;

            let fvk_bytes = &FullViewingKey::encode_to_vec(&fvk)[..];
            tx.execute(
                "INSERT INTO full_viewing_key (bytes) VALUES (?1)",
                [fvk_bytes],
            )?;

            // Insert -1 as a signaling value for pre-genesis.
            // We just have to be careful to treat negative values as None
            // in last_sync_height.
            tx.execute("INSERT INTO sync_height (height) VALUES (-1)", ())?;

            // Insert the schema hash into the database
            tx.execute(
                "INSERT INTO schema_hash (schema_hash) VALUES (?1)",
                [&*SCHEMA_HASH],
            )?;

            // Insert the client version into the database
            tx.execute(
                "INSERT INTO client_version (client_version) VALUES (?1)",
                [env!("VERGEN_GIT_SEMVER")],
            )?;

            tx.commit()?;
            drop(conn);

            Ok(Storage {
                pool,
                uncommitted_height: Arc::new(Mutex::new(None)),
                scanned_notes_tx: broadcast::channel(128).0,
                scanned_nullifiers_tx: broadcast::channel(512).0,
                scanned_swaps_tx: broadcast::channel(128).0,
            })
        })
        .await?
    }

    /// Query for account balance by address
    pub async fn balance_by_address(&self, address: Address) -> anyhow::Result<BTreeMap<Id, u128>> {
        let pool = self.pool.clone();

        spawn_blocking(move || {
            let address = address.to_vec();

            let mut balance_by_address = BTreeMap::new();

            for result in pool.get()?
                .prepare_cached(
                    "SELECT notes.asset_id, notes.amount
                    FROM    notes
                    JOIN    spendable_notes ON notes.note_commitment = spendable_notes.note_commitment
                    WHERE   spendable_notes.height_spent IS NULL
                    AND     notes.address IS ?1",
                )?
                .query_map([address], |row| {
                    let asset_id: Vec<u8> = row.get("asset_id")?;
                    let amount: [u8; 16] = row.get("amount")?;
                    Ok((asset_id, amount))
                })? {

                let (asset_id, amount) = result?;

                let amount_u128: u128 = u128::from_be_bytes(amount);

                balance_by_address
                    .entry(Id::try_from(asset_id.as_slice())?)
                    .and_modify(|x| *x += amount_u128)
                    .or_insert(amount_u128);
            }

            Ok(balance_by_address)
        })
        .await?
    }

    /// Query for a note by its note commitment, optionally waiting until the note is detected.
    pub async fn note_by_commitment(
        &self,
        note_commitment: tct::Commitment,
        await_detection: bool,
    ) -> anyhow::Result<SpendableNoteRecord> {
        // Start subscribing now, before querying for whether we already
        // have the record, so that we can't miss it if we race a write.
        let mut rx = self.scanned_notes_tx.subscribe();

        let pool = self.pool.clone();

        if let Some(record) = spawn_blocking(move || {
            // Check if we already have the record
            pool.get()?
                .prepare(&format!(
                    "SELECT
                        notes.note_commitment,
                        spendable_notes.height_created,
                        notes.address,
                        notes.amount,
                        notes.asset_id,
                        notes.rseed,
                        spendable_notes.address_index,
                        spendable_notes.source,
                        spendable_notes.height_spent,
                        spendable_notes.nullifier,
                        spendable_notes.position
                    FROM notes
                    JOIN spendable_notes ON notes.note_commitment = spendable_notes.note_commitment
                    WHERE notes.note_commitment = x'{}'",
                    hex::encode(note_commitment.0.to_bytes())
                ))?
                .query_and_then((), |record| record.try_into())?
                .next()
                .transpose()
        })
        .await??
        {
            return Ok(record);
        }

        if !await_detection {
            return Err(anyhow!("Note commitment {} not found", note_commitment));
        }

        // Otherwise, wait for newly detected notes and check whether they're
        // the requested one.

        loop {
            match rx.recv().await {
                Ok(record) => {
                    if record.note_commitment == note_commitment {
                        return Ok(record);
                    }
                }

                Err(e) => match e {
                    RecvError::Closed => {
                        return Err(anyhow!(
                            "Receiver error during note detection: closed (no more active senders)"
                        ))
                    }
                    RecvError::Lagged(count) => {
                        return Err(anyhow!(
                            "Receiver error during note detection: lagged (by {:?} messages)",
                            count
                        ))
                    }
                },
            };
        }
    }

    /// Query for a swap by its swap commitment, optionally waiting until the note is detected.
    pub async fn swap_by_commitment(
        &self,
        swap_commitment: tct::Commitment,
        await_detection: bool,
    ) -> anyhow::Result<SwapRecord> {
        // Start subscribing now, before querying for whether we already
        // have the record, so that we can't miss it if we race a write.
        let mut rx = self.scanned_swaps_tx.subscribe();

        let pool = self.pool.clone();

        if let Some(record) = spawn_blocking(move || {
            // Check if we already have the swap record
            pool.get()?
                .prepare(&format!(
                    "SELECT * FROM swaps WHERE swaps.swap_commitment = x'{}'",
                    hex::encode(swap_commitment.0.to_bytes())
                ))?
                .query_and_then((), |record| record.try_into())?
                .next()
                .transpose()
        })
        .await??
        {
            return Ok(record);
        }

        if !await_detection {
            return Err(anyhow!("swap commitment {} not found", swap_commitment));
        }

        // Otherwise, wait for newly detected swaps and check whether they're
        // the requested one.

        loop {
            match rx.recv().await {
                Ok(record) => {
                    if record.swap_commitment == swap_commitment {
                        return Ok(record);
                    }
                }

                Err(e) => match e {
                    RecvError::Closed => {
                        return Err(anyhow!(
                            "Receiver error during swap detection: closed (no more active senders)"
                        ))
                    }
                    RecvError::Lagged(count) => {
                        return Err(anyhow!(
                            "Receiver error during swap detection: lagged (by {:?} messages)",
                            count
                        ))
                    }
                },
            };
        }
    }

    /// Query for a nullifier's status, optionally waiting until the nullifier is detected.
    pub async fn nullifier_status(
        &self,
        nullifier: Nullifier,
        await_detection: bool,
    ) -> anyhow::Result<bool> {
        // Start subscribing now, before querying for whether we already have the nullifier, so we
        // can't miss it if we race a write.
        let mut rx = self.scanned_nullifiers_tx.subscribe();

        // Clone the pool handle so that the returned future is 'static
        let pool = self.pool.clone();

        let nullifier_bytes = nullifier.0.to_bytes().to_vec();

        // Check if we already have the nullifier in the set of spent notes
        if let Some(height_spent) = spawn_blocking(move || {
            pool.get()?
                .prepare_cached("SELECT height_spent FROM spendable_notes WHERE nullifier = ?1")?
                .query_and_then([nullifier_bytes], |row| {
                    let height_spent: Option<u64> = row.get("height_spent")?;
                    Ok::<_, anyhow::Error>(height_spent)
                })?
                .next()
                .transpose()
        })
        .await??
        {
            let spent = height_spent.is_some();

            // If we're awaiting detection and the nullifier isn't yet spent, don't return just yet
            if !await_detection || spent {
                return Ok(spent);
            }
        }

        // After checking the database, if we didn't find it, return `false` unless we are to
        // await detection
        if !await_detection {
            return Ok(false);
        }

        // Otherwise, wait for newly detected nullifiers and check whether they're the requested
        // one.
        loop {
            let new_nullifier = rx.recv().await.context("change subscriber failed")?;

            if new_nullifier == nullifier {
                return Ok(true);
            }
        }
    }

    /// The last block height we've scanned to, if any.
    pub async fn last_sync_height(&self) -> anyhow::Result<Option<u64>> {
        // Check if we have uncommitted blocks beyond the database height.
        if let Some(height) = *self.uncommitted_height.lock() {
            return Ok(Some(height.get()));
        }

        let pool = self.pool.clone();

        spawn_blocking(move || {
            let height: Option<i64> = pool
                .get()?
                .prepare_cached("SELECT height FROM sync_height ORDER BY height DESC LIMIT 1")?
                .query_row([], |row| row.get::<_, Option<i64>>(0))?;

            Ok::<_, anyhow::Error>(
                u64::try_from(height.ok_or_else(|| anyhow!("missing sync height"))?).ok(),
            )
        })
        .await?
    }

    pub async fn chain_params(&self) -> anyhow::Result<ChainParameters> {
        let pool = self.pool.clone();

        spawn_blocking(move || {
            let bytes = pool
                .get()?
                .prepare_cached("SELECT bytes FROM chain_params LIMIT 1")?
                .query_row([], |row| row.get::<_, Option<Vec<u8>>>("bytes"))?
                .ok_or_else(|| anyhow!("missing chain params"))?;

            ChainParameters::decode(bytes.as_slice())
        })
        .await?
    }

    pub async fn fmd_parameters(&self) -> anyhow::Result<FmdParameters> {
        let pool = self.pool.clone();

        spawn_blocking(move || {
            let bytes = pool
                .get()?
                .prepare_cached("SELECT bytes FROM fmd_parameters LIMIT 1")?
                .query_row([], |row| row.get::<_, Option<Vec<u8>>>("bytes"))?
                .ok_or_else(|| anyhow!("missing fmd parameters"))?;

            FmdParameters::decode(bytes.as_slice())
        })
        .await?
    }

    pub async fn full_viewing_key(&self) -> anyhow::Result<FullViewingKey> {
        let pool = self.pool.clone();

        spawn_blocking(move || {
            let bytes = pool
                .get()?
                .prepare_cached("SELECT bytes FROM full_viewing_key LIMIT 1")?
                .query_row([], |row| row.get::<_, Option<Vec<u8>>>("bytes"))?
                .ok_or_else(|| anyhow!("missing full viewing key"))?;

            FullViewingKey::decode(bytes.as_slice())
        })
        .await?
    }

    pub async fn state_commitment_tree(&self) -> anyhow::Result<tct::Tree> {
        let pool = self.pool.clone();
        spawn_blocking(move || {
            tct::Tree::from_reader(&mut TreeStore(&mut pool.get()?.transaction()?))
        })
        .await?
    }

    /// Returns a tuple of (block height, transaction hash) for all transactions in a given range of block heights.
    pub async fn transaction_hashes(
        &self,
        start_height: Option<u64>,
        end_height: Option<u64>,
    ) -> anyhow::Result<Vec<(u64, Vec<u8>)>> {
        let starting_block = start_height.unwrap_or(0) as i64;
        let ending_block = end_height.unwrap_or(self.last_sync_height().await?.unwrap_or(0)) as i64;

        let pool = self.pool.clone();

        spawn_blocking(move || {
            pool.get()?
                .prepare_cached(
                    "SELECT block_height, tx_hash
                    FROM tx
                    WHERE block_height BETWEEN ?1 AND ?2",
                )?
                .query_and_then([starting_block, ending_block], |row| {
                    let block_height: u64 = row.get("block_height")?;
                    let tx_hash: Vec<u8> = row.get("tx_hash")?;
                    Ok::<_, anyhow::Error>((block_height, tx_hash))
                })?
                .collect()
        })
        .await?
    }

    /// Returns a tuple of (block height, transaction hash, transaction) for all transactions in a given range of block heights.
    pub async fn transactions(
        &self,
        start_height: Option<u64>,
        end_height: Option<u64>,
    ) -> anyhow::Result<Vec<(u64, Vec<u8>, Transaction)>> {
        let starting_block = start_height.unwrap_or(0) as i64;
        let ending_block = end_height.unwrap_or(self.last_sync_height().await?.unwrap_or(0)) as i64;

        let pool = self.pool.clone();

        spawn_blocking(move || {
            pool.get()?
                .prepare_cached(
                    "SELECT block_height, tx_hash, tx_bytes
                    FROM tx
                    WHERE block_height BETWEEN ?1 AND ?2",
                )?
                .query_and_then([starting_block, ending_block], |row| {
                    let block_height: u64 = row.get("block_height")?;
                    let tx_hash: Vec<u8> = row.get("tx_hash")?;
                    let tx_bytes: Vec<u8> = row.get("tx_bytes")?;
                    let tx = Transaction::decode(tx_bytes.as_slice())?;
                    Ok::<_, anyhow::Error>((block_height, tx_hash, tx))
                })?
                .collect()
        })
        .await?
    }

    pub async fn transaction_by_hash(
        &self,
        tx_hash: &[u8],
    ) -> anyhow::Result<Option<(u64, Transaction)>> {
        let pool = self.pool.clone();
        let tx_hash = tx_hash.to_vec();

        spawn_blocking(move || {
            if let Some((block_height, tx_bytes)) = pool
                .get()?
                .prepare_cached("SELECT block_height, tx_bytes FROM tx WHERE tx_hash = ?1")?
                .query_row([tx_hash], |row| {
                    let block_height: u64 = row.get("block_height")?;
                    let tx_bytes: Vec<u8> = row.get("tx_bytes")?;
                    Ok((block_height, tx_bytes))
                })
                .optional()?
            {
                let tx = Transaction::decode(tx_bytes.as_slice())?;
                Ok(Some((block_height, tx)))
            } else {
                Ok(None)
            }
        })
        .await?
    }

    // Query for a note by its note commitment, optionally waiting until the note is detected.
    pub async fn note_by_nullifier(
        &self,
        nullifier: Nullifier,
        await_detection: bool,
    ) -> anyhow::Result<SpendableNoteRecord> {
        // Start subscribing now, before querying for whether we already
        // have the record, so that we can't miss it if we race a write.
        let mut rx = self.scanned_notes_tx.subscribe();

        // Clone the pool handle so that the returned future is 'static
        let pool = self.pool.clone();

        let nullifier_bytes = nullifier.to_bytes().to_vec();

        if let Some(record) = spawn_blocking(move || {
            let record = pool
                .get()?
                .prepare(&format!(
                    "SELECT
                        notes.note_commitment,
                        spendable_notes.height_created,
                        notes.address,
                        notes.amount,
                        notes.asset_id,
                        notes.rseed,
                        spendable_notes.address_index,
                        spendable_notes.source,
                        spendable_notes.height_spent,
                        spendable_notes.nullifier,
                        spendable_notes.position
                    FROM notes
                    JOIN spendable_notes ON notes.note_commitment = spendable_notes.note_commitment
                    WHERE hex(spendable_notes.nullifier) = \"{}\"",
                    hex::encode_upper(nullifier_bytes)
                ))?
                .query_and_then((), |row| SpendableNoteRecord::try_from(row))?
                .next()
                .transpose()?;

            Ok::<_, anyhow::Error>(record)
        })
        .await??
        {
            return Ok(record);
        }

        if !await_detection {
            return Err(anyhow!(
                "Note commitment for nullifier {:?} not found",
                nullifier
            ));
        }

        // Otherwise, wait for newly detected notes and check whether they're
        // the requested one.

        loop {
            match rx.recv().await {
                Ok(record) => {
                    if record.nullifier == nullifier {
                        return Ok(record);
                    }
                }

                Err(e) => match e {
                    RecvError::Closed => {
                        return Err(anyhow!(
                            "Receiver error during note detection: closed (no more active senders)"
                        ))
                    }
                    RecvError::Lagged(count) => {
                        return Err(anyhow!(
                            "Receiver error during note detection: lagged (by {:?} messages)",
                            count
                        ))
                    }
                },
            };
        }
    }

    pub async fn all_assets(&self) -> anyhow::Result<Vec<Asset>> {
        let pool = self.pool.clone();

        spawn_blocking(move || {
            pool.get()?
                .prepare_cached("SELECT * FROM assets")?
                .query_and_then([], |row| {
                    let asset_id: Vec<u8> = row.get("asset_id")?;
                    let denom: String = row.get("denom")?;
                    let asset = Asset {
                        id: Id::try_from(asset_id.as_slice())?,
                        denom: asset::REGISTRY
                            .parse_denom(&denom)
                            .ok_or_else(|| anyhow::anyhow!("invalid denomination {}", denom))?,
                    };
                    Ok::<_, anyhow::Error>(asset)
                })?
                .collect()
        })
        .await?
    }

    pub async fn asset_by_id(&self, id: &Id) -> anyhow::Result<Option<Asset>> {
        let id = id.to_bytes().to_vec();

        let pool = self.pool.clone();

        spawn_blocking(move || {
            pool.get()?
                .prepare_cached("SELECT * FROM assets WHERE asset_id = ?1")?
                .query_and_then([id], |row| {
                    let asset_id: Vec<u8> = row.get("asset_id")?;
                    let denom: String = row.get("denom")?;
                    let asset = Asset {
                        id: Id::try_from(asset_id.as_slice())?,
                        denom: asset::REGISTRY
                            .parse_denom(&denom)
                            .ok_or_else(|| anyhow::anyhow!("invalid denomination {}", denom))?,
                    };
                    Ok::<_, anyhow::Error>(asset)
                })?
                .next()
                .transpose()
        })
        .await?
    }

    // Get assets whose denoms match the given SQL LIKE pattern, with the `_` and `%` wildcards,
    // where `\` is the escape character.
    pub async fn assets_matching(&self, pattern: String) -> anyhow::Result<Vec<Asset>> {
        let pattern = pattern.to_owned();

        let pool = self.pool.clone();

        spawn_blocking(move || {
            pool.get()?
                .prepare_cached("SELECT * FROM assets WHERE denom LIKE ?1 ESCAPE '\\'")?
                .query_and_then([pattern], |row| {
                    let asset_id: Vec<u8> = row.get("asset_id")?;
                    let denom: String = row.get("denom")?;
                    let asset = Asset {
                        id: Id::try_from(asset_id.as_slice())?,
                        denom: asset::REGISTRY
                            .parse_denom(&denom)
                            .ok_or_else(|| anyhow::anyhow!("invalid denomination {}", denom))?,
                    };
                    Ok::<_, anyhow::Error>(asset)
                })?
                .collect()
        })
        .await?
    }

    pub async fn notes(
        &self,
        include_spent: bool,
        asset_id: Option<asset::Id>,
        address_index: Option<penumbra_crypto::keys::AddressIndex>,
        amount_to_spend: Option<Amount>,
    ) -> anyhow::Result<Vec<SpendableNoteRecord>> {
        // If set, return spent notes as well as unspent notes.
        // bool include_spent = 2;
        let spent_clause = match include_spent {
            false => "NULL",
            true => "height_spent",
        };

        // If set, only return notes with the specified asset id.
        // core.crypto.v1alpha1.AssetId asset_id = 3;
        let asset_clause = asset_id
            .map(|id| format!("x'{}'", hex::encode(id.to_bytes())))
            .unwrap_or_else(|| "asset_id".to_string());

        // If set, only return notes with the specified address index.
        // crypto.AddressIndex address_index = 4;
        // This isn't what we want any more, we need to be indexing notes
        // by *account*, not just by address index.
        // For now, just do filtering in software.
        /*
        let address_clause = address_index
            .map(|d| format!("x'{}'", hex::encode(d.to_bytes())))
            .unwrap_or_else(|| "address_index".to_string());
         */
        let address_clause = "address_index".to_string();

        // If set, stop returning notes once the total exceeds this amount.
        //
        // Ignored if `asset_id` is unset or if `include_spent` is set.
        // uint64 amount_to_spend = 5;
        //TODO: figure out a clever way to only return notes up to the sum using SQL
        let amount_cutoff = (amount_to_spend.is_some()) && !(include_spent || asset_id.is_none());
        let mut amount_total = Amount::zero();

        let pool = self.pool.clone();

        spawn_blocking(move || {
            let mut output: Vec<SpendableNoteRecord> = Vec::new();

            for result in pool
                .get()?
                .prepare(&format!(
                    "SELECT notes.note_commitment,
                        spendable_notes.height_created,
                        notes.address,
                        notes.amount,
                        notes.asset_id,
                        notes.rseed,
                        spendable_notes.address_index,
                        spendable_notes.source,
                        spendable_notes.height_spent,
                        spendable_notes.nullifier,
                        spendable_notes.position
                FROM notes
                JOIN spendable_notes ON notes.note_commitment = spendable_notes.note_commitment
                WHERE spendable_notes.height_spent IS {spent_clause}
                AND notes.asset_id IS {asset_clause}
                AND spendable_notes.address_index IS {address_clause}"
                ))?
                .query_and_then((), |row| SpendableNoteRecord::try_from(row))?
            {
                let record = result?;

                // Skip notes that don't match the account, since we're
                // not doing account filtering in SQL as a temporary hack (see above)
                if let Some(address_index) = address_index {
                    if record.address_index.account != address_index.account {
                        continue;
                    }
                }
                let amount = record.note.amount();
                output.push(record);
                // If we're tracking amounts, accumulate the value of the note
                // and check if we should break out of the loop.
                if amount_cutoff {
                    // We know all the notes are of the same type, so adding raw quantities makes sense.
                    amount_total = amount_total + amount;
                    if amount_total >= amount_to_spend.unwrap_or_default() {
                        break;
                    }
                }
            }

            if amount_total < amount_to_spend.unwrap_or_default() {
                return Err(anyhow!(
                    "requested amount of {} exceeds total of {}",
                    amount_to_spend.unwrap_or_default(),
                    amount_total
                ));
            }

            Ok::<_, anyhow::Error>(output)
        })
        .await?
    }

    pub async fn notes_for_voting(
        &self,
        address_index: Option<penumbra_crypto::keys::AddressIndex>,
        votable_at_height: u64,
    ) -> anyhow::Result<Vec<(SpendableNoteRecord, IdentityKey)>> {
        // If set, only return notes with the specified address index.
        // crypto.AddressIndex address_index = 3;
        let address_clause = address_index
            .map(|d| format!("x'{}'", hex::encode(d.to_bytes())))
            .unwrap_or_else(|| "address_index".to_string());

        let pool = self.pool.clone();

        spawn_blocking(move || {
            let mut lock = pool.get()?;
            let dbtx = lock.transaction()?;

            let spendable_note_records: Vec<SpendableNoteRecord> = dbtx
                .prepare(&format!(
                    "SELECT notes.note_commitment,
                        spendable_notes.height_created,
                        notes.address,
                        notes.amount,
                        notes.asset_id,
                        notes.rseed,
                        spendable_notes.address_index,
                        spendable_notes.source,
                        spendable_notes.height_spent,
                        spendable_notes.nullifier,
                        spendable_notes.position
                    FROM
                        notes JOIN spendable_notes ON notes.note_commitment = spendable_notes.note_commitment
                    WHERE
                        spendable_notes.address_index IS {address_clause}
                        AND notes.asset_id IN (
                            SELECT asset_id FROM assets WHERE denom LIKE '_delegation\\_%' ESCAPE '\\'
                        )
                        AND ((spendable_notes.height_spent IS NULL) OR (spendable_notes.height_spent > {votable_at_height}))
                        AND (spendable_notes.height_created < {votable_at_height})
                    ",
                ))?
                .query_and_then((), |row| row.try_into())?
                .collect::<Result<Vec<_>, anyhow::Error>>()?;

            // TODO: this could be internalized into the SQL query in principle, but it's easier to
            // do it this way; if it becomes slow, we can do it better
            let mut results = Vec::new();
            for record in spendable_note_records {
                let asset_id = record.note.asset_id().to_bytes().to_vec();
                let denom: String = dbtx.query_row_and_then(
                    "SELECT denom FROM assets WHERE asset_id = ?1",
                    [asset_id],
                    |row| row.get("asset_id")
                )?;

                let identity_key = DelegationToken::from_str(&denom)
                    .context("invalid delegation token denom")?
                    .validator();

                results.push((record, identity_key));
            }

            Ok(results)

        }).await?
    }

    pub async fn record_asset(&self, asset: Asset) -> anyhow::Result<()> {
        let asset_id = asset.id.to_bytes().to_vec();
        let denom = asset.denom.to_string();

        let pool = self.pool.clone();

        spawn_blocking(move || {
            pool.get()?
                .execute(
                    "INSERT OR IGNORE INTO assets (asset_id, denom) VALUES (?1, ?2)",
                    (asset_id, denom),
                )
                .map_err(anyhow::Error::from)
        })
        .await??;

        Ok(())
    }

    pub async fn record_empty_block(&self, height: u64) -> anyhow::Result<()> {
        // Check that the incoming block height follows the latest recorded height
        let last_sync_height = self.last_sync_height().await?.ok_or_else(|| {
            anyhow::anyhow!("invalid: tried to record empty block as genesis block")
        })?;

        if height != last_sync_height + 1 {
            return Err(anyhow::anyhow!(
                "Wrong block height {} for latest sync height {}",
                height,
                last_sync_height
            ));
        }

        *self.uncommitted_height.lock() = Some(height.try_into().unwrap());
        Ok(())
    }

    fn record_note_inner(
        dbtx: &r2d2_sqlite::rusqlite::Transaction<'_>,
        note: &Note,
    ) -> anyhow::Result<()> {
        let note_commitment = note.commit().0.to_bytes().to_vec();
        let address = note.address().to_vec();
        let amount = u128::from(note.amount()).to_be_bytes().to_vec();
        let asset_id = note.asset_id().to_bytes().to_vec();
        let rseed = note.rseed().to_bytes().to_vec();

        dbtx.execute(
            "INSERT INTO notes (note_commitment, address, amount, asset_id, rseed) 
                VALUES (?1, ?2, ?3, ?4, ?5) 
                ON CONFLICT DO NOTHING",
            (note_commitment, address, amount, asset_id, rseed),
        )?;

        Ok(())
    }

    pub async fn give_advice(&self, note: Note) -> anyhow::Result<()> {
        // Do not insert advice for zero amounts, simply return Ok because this is fine
        if u128::from(note.amount()) == 0u128 {
            return Ok(());
        }

        let pool = self.pool.clone();
        let mut lock = pool.get()?;
        let dbtx = lock.transaction()?;

        Storage::record_note_inner(&dbtx, &note)?;

        dbtx.commit()?;

        Ok(())
    }

    /// Return advice about note contents for use in scanning.
    ///
    /// Given a list of note commitments, this method checks whether any of them
    /// correspond to notes that have been recorded in the database but not yet
    /// observed during scanning.
    pub async fn scan_advice(
        &self,
        note_commitments: Vec<note::Commitment>,
    ) -> anyhow::Result<BTreeMap<note::Commitment, Note>> {
        if note_commitments.is_empty() {
            return Ok(BTreeMap::new());
        }

        let pool = self.pool.clone();

        // This query gives advice about notes which are known but which have not already been recorded as spendable,
        // in part to avoid revealing information about which notes have been spent.

        spawn_blocking(move || {
            pool.get()?
                .prepare(&format!(
                    "SELECT notes.note_commitment,
                        notes.address,
                        notes.amount,
                        notes.asset_id,
                        notes.rseed
                    FROM notes
                    LEFT OUTER JOIN spendable_notes ON notes.note_commitment = spendable_notes.note_commitment
                    WHERE (spendable_notes.note_commitment IS NULL) AND (notes.note_commitment IN ({}))",
                    note_commitments
                        .iter()
                        .map(|cm| format!("x'{}'", hex::encode(cm.0.to_bytes())))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))?
                .query_and_then((), |row| {
                    let address = Address::try_from(row.get::<_, Vec<u8>>("address")?)?;
                    let amount = row.get::<_, [u8; 16]>("amount")?;
                    let amount_u128: u128 = u128::from_be_bytes(amount);
                    let asset_id = asset::Id(Fq::from_bytes(row.get::<_, [u8; 32]>("asset_id")?)?);
                    let rseed = Rseed(row.get::<_, [u8; 32]>("rseed")?);
                    let note = Note::from_parts(
                        address,
                        Value {
                            amount: amount_u128.into(),
                            asset_id,
                        },
                        rseed,
                    )?;
                    Ok::<_, anyhow::Error>((note.commit(), note))
                })?
                .collect::<Result<BTreeMap<_, _>, anyhow::Error>>()
        }).await?
    }

    /// Filters for nullifiers whose notes we control
    pub async fn filter_nullifiers(
        &self,
        nullifiers: Vec<Nullifier>,
    ) -> anyhow::Result<Vec<Nullifier>> {
        if nullifiers.is_empty() {
            return Ok(Vec::new());
        }

        let pool = self.pool.clone();

        spawn_blocking(move || {
            pool.get()?
                .prepare(&format!(
                    "SELECT nullifier FROM spendable_notes WHERE nullifier IN ({})",
                    nullifiers
                        .iter()
                        .map(|x| format!("x'{}'", hex::encode(x.0.to_bytes())))
                        .collect::<Vec<String>>()
                        .join(",")
                ))?
                .query_and_then((), |row| {
                    let nullifier: Vec<u8> = row.get("nullifier")?;
                    nullifier.as_slice().try_into()
                })?
                .collect()
        })
        .await?
    }

    pub async fn record_block(
        &self,
        filtered_block: FilteredBlock,
        transactions: Vec<Transaction>,
        sct: &mut tct::Tree,
    ) -> anyhow::Result<()> {
        //Check that the incoming block height follows the latest recorded height
        let last_sync_height = self.last_sync_height().await?;

        let correct_height = match last_sync_height {
            // Require that the new block follows the last one we scanned.
            Some(cur_height) => filtered_block.height == cur_height + 1,
            // Require that the new block represents the initial chain state.
            None => filtered_block.height == 0,
        };

        if !correct_height {
            return Err(anyhow::anyhow!(
                "Wrong block height {} for latest sync height {:?}",
                filtered_block.height,
                last_sync_height
            ));
        }

        let pool = self.pool.clone();
        let uncommitted_height = self.uncommitted_height.clone();
        let scanned_notes_tx = self.scanned_notes_tx.clone();
        let scanned_nullifiers_tx = self.scanned_nullifiers_tx.clone();
        let scanned_swaps_tx = self.scanned_swaps_tx.clone();

        // Cloning the SCT is cheap because it's a copy-on-write structure, so we move an owned copy
        // into the spawned thread. This means that if for any reason the thread panics or throws an
        // error, the changes to the SCT will be discarded, just like any changes to the database,
        // so the two stay transactionally in sync, even in the case of errors. This would not be
        // the case if we `std::mem::take` the SCT and move it into the spawned thread, because then
        // an error would mean the updated version would never be put back, and the outcome would be
        // a cleared SCT but a non-empty database.
        let mut new_sct = sct.clone();

        *sct = spawn_blocking(move || {
            let mut lock = pool.get()?;
            let mut dbtx = lock.transaction()?;

            // If the chain parameters have changed, update them.
            if let Some(params) = filtered_block.chain_parameters {
                let chain_params_bytes = &ChainParameters::encode_to_vec(&params)[..];
                dbtx.execute(
                    "INSERT INTO chain_params (bytes) VALUES (?1)",
                    [chain_params_bytes],
                )?;
            }

            // Insert new note records into storage
            for note_record in &filtered_block.new_notes {
                let note_commitment = note_record.note_commitment.0.to_bytes().to_vec();
                let height_created = filtered_block.height as i64;
                let address_index = note_record.address_index.to_bytes().to_vec();
                let nullifier = note_record.nullifier.to_bytes().to_vec();
                let position = (u64::from(note_record.position)) as i64;
                let source = note_record.source.to_bytes().to_vec();

                // Record the inner note data in the notes table

                Storage::record_note_inner(&dbtx,&note_record.note)?;

                dbtx.execute(
                    "INSERT INTO spendable_notes
                    (note_commitment, nullifier, position, height_created, address_index, source, height_spent)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                    (
                        &note_commitment,
                        &nullifier,
                        &position,
                        &height_created,
                        &address_index,
                        &source,
                        // height_spent is NULL because the note is newly discovered
                    ),
                )?;
            }

            // Insert new swap records into storage
            for swap in &filtered_block.new_swaps {
                let swap_commitment = swap.swap_commitment.0.to_bytes().to_vec();
                let swap_bytes = swap.swap.encode_to_vec();
                let position = (u64::from(swap.position)) as i64;
                let nullifier = swap.nullifier.to_bytes().to_vec();
                let source = swap.source.to_bytes().to_vec();
                let output_data = swap.output_data.encode_to_vec();

                dbtx.execute(
                    "INSERT INTO swaps (swap_commitment, swap, position, nullifier, output_data, height_claimed, source)
                    VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)",
                    (
                        &swap_commitment,
                        &swap_bytes,
                        &position,
                        &nullifier,
                        &output_data,
                        // height_claimed is NULL because the swap is newly discovered
                        &source,
                    ),
                )?;
            }

            // Update any rows of the table with matching nullifiers to have height_spent
            for nullifier in &filtered_block.spent_nullifiers {
                let height_spent = filtered_block.height as i64;
                let nullifier = nullifier.to_bytes().to_vec();

                let spent_commitment: Option<Commitment> = dbtx.prepare_cached(
                    "UPDATE spendable_notes SET height_spent = ?1 WHERE nullifier = ?2 RETURNING note_commitment"
                )?
                .query_and_then(
                    (height_spent, &nullifier),
                    |row| {
                        let bytes: Vec<u8> = row.get("note_commitment")?;
                        Commitment::try_from(&bytes[..]).context("invalid commitment bytes")
                    }
                )?
                .next()
                .transpose()?;

                let swap_commitment: Option<Commitment> = dbtx.prepare_cached(
                        "UPDATE swaps SET height_claimed = ?1 WHERE nullifier = ?2 RETURNING swap_commitment"
                    )?
                    .query_and_then(
                        (height_spent, &nullifier),
                        |row| {
                            let bytes: Vec<u8> = row.get("swap_commitment")?;
                            Commitment::try_from(&bytes[..]).context("invalid commitment bytes")
                        }
                    )?
                    .next()
                    .transpose()?;

                // Check denom type
                let spent_denom: String = dbtx.prepare_cached(
                        "SELECT assets.denom
                        FROM spendable_notes JOIN notes LEFT JOIN assets ON notes.asset_id == assets.asset_id
                        WHERE nullifier = ?1"
                    )?
                    .query_and_then(
                        [&nullifier],
                        |row| row.get("denom")
                    )?
                    .next()
                    .transpose()?
                    .ok_or_else(|| anyhow!("denom must exist for note we know about"))?;

                // Mark spent notes as spent
                if let Some(spent_commitment) = spent_commitment {
                    // Forget spent note commitments from the SCT unless they are delegation tokens,
                    // which must be saved to allow voting on proposals that might or might not be
                    // open presently

                    if DelegationToken::from_str(&spent_denom).is_ok() {
                        new_sct.forget(spent_commitment);
                    }
                };

                // Mark spent swaps as spent
                if let Some(spent_swap_commitment) = swap_commitment {
                    // Forget spent swap commitments from the SCT unless they are delegation tokens,
                    // which must be saved to allow voting on proposals that might or might not be open
                    // presently

                    if DelegationToken::from_str(&spent_denom).is_ok() {
                        new_sct.forget(spent_swap_commitment);
                    }
                };
            }

            // Update SCT table with current SCT state
            new_sct.to_writer(&mut TreeStore(&mut dbtx))?;

            // Record all transactions
            for transaction in transactions {
                let tx_bytes = transaction.encode_to_vec();
                // We have to create an explicit temporary borrow, because the sqlx api is bad (see above)
                let tx_hash_owned = sha2::Sha256::digest(&tx_bytes);
                let tx_hash = tx_hash_owned.as_slice();
                let tx_block_height = filtered_block.height as i64;

                tracing::debug!(tx_hash = ?hex::encode(tx_hash), "recording extended transaction");

                dbtx.execute(
                    "INSERT INTO tx (tx_hash, tx_bytes, block_height) VALUES (?1, ?2, ?3)",
                    (&tx_hash, &tx_bytes, tx_block_height),
                )?;

                // Associate all of the spent nullifiers with the transaction by hash.
                for nf in transaction.spent_nullifiers() {
                    let nf_bytes = nf.0.to_bytes().to_vec();
                    dbtx.execute(
                        "INSERT INTO tx_by_nullifier (nullifier, tx_hash) VALUES (?1, ?2)",
                        (&nf_bytes, &tx_hash),
                    )?;
                }
            }

            // Update FMD parameters if they've changed.
            if filtered_block.fmd_parameters.is_some() {
                let fmd_parameters_bytes =
                    &FmdParameters::encode_to_vec(&filtered_block.fmd_parameters.unwrap())[..];

                dbtx.execute("INSERT INTO fmd_parameters (bytes) VALUES (?1)", [&fmd_parameters_bytes])?;
            }

            // Record block height as latest synced height
            let latest_sync_height = filtered_block.height as i64;
            dbtx.execute("UPDATE sync_height SET height = ?1", [latest_sync_height])?;

            // Commit the changes to the database
            dbtx.commit()?;

            // IMPORTANT: NO PANICS OR ERRORS PAST THIS POINT
            // If there is a panic or error past this point, the database will be left in out of
            // sync with the in-memory copy of the SCT, which means that it will become corrupted as
            // synchronization continues.

            // It's critical to reset the uncommitted height here, since we've just
            // invalidated it by committing.
            uncommitted_height.lock().take();

            // Broadcast all committed note records to channel
            // Done following tx.commit() to avoid notifying of a new SpendableNoteRecord before it is actually committed to the database

            for note_record in &filtered_block.new_notes {
                // This will fail to be broadcast if there is no active receiver (such as on initial
                // sync) The error is ignored, as this isn't a problem, because if there is no
                // active receiver there is nothing to do
                let _ = scanned_notes_tx.send(note_record.clone());
            }

            for nullifier in filtered_block.spent_nullifiers.iter() {
                // This will fail to be broadcast if there is no active receiver (such as on initial
                // sync) The error is ignored, as this isn't a problem, because if there is no
                // active receiver there is nothing to do
                let _ = scanned_nullifiers_tx.send(*nullifier);
            }

            for swap_record in filtered_block.new_swaps {
                // This will fail to be broadcast if there is no active rece∑iver (such as on initial
                // sync) The error is ignored, as this isn't a problem, because if there is no
                // active receiver there is nothing to do
                let _ = scanned_swaps_tx.send(swap_record.clone());
            }

            Ok::<_, anyhow::Error>(new_sct)
        })
        .await??;

        Ok(())
    }

    pub async fn owned_position_ids(
        &self,
        position_state: Option<State>,
        trading_pair: Option<TradingPair>,
    ) -> anyhow::Result<Vec<position::Id>> {
        let pool = self.pool.clone();

        let state_clause = match position_state {
            Some(state) => state.to_string(),
            None => "position_state".to_string(),
        };

        let pair_clause = match trading_pair {
            Some(pair) => pair.to_string(),
            None => "trading_pair".to_string(),
        };

        spawn_blocking(move || {
            pool.get()?
                .prepare_cached(
                    "SELECT position_id FROM positions 
                    WHERE position_state = ?1 
                    AND trading_pair = ?2",
                )?
                .query_and_then([state_clause, pair_clause], |row| {
                    let position_id: Vec<u8> = row.get("position_id")?;

                    position::Id::decode(position_id.as_slice())
                })?
                .collect()
        })
        .await?
    }
}
