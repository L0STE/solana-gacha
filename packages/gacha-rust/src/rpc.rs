//! Thin read-only RPC composition. Wallets and transaction submission stay external.

use super::*;
use solana_account_decoder_client_types::{UiAccountEncoding, UiDataSliceConfig};
use solana_rpc_client::api::{client_error, config::RpcAccountInfoConfig};
pub use solana_rpc_client::nonblocking::rpc_client::RpcClient;

#[derive(Debug)]
pub enum RpcError {
    Transport(client_error::Error),
    Account(Error),
    MissingAccount(Pubkey),
}

impl core::fmt::Display for RpcError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Transport(error) => error.fmt(f),
            Self::Account(error) => error.fmt(f),
            Self::MissingAccount(key) => write!(f, "missing account {key}"),
        }
    }
}
impl std::error::Error for RpcError {}
impl From<client_error::Error> for RpcError {
    fn from(e: client_error::Error) -> Self {
        Self::Transport(e)
    }
}
impl From<Error> for RpcError {
    fn from(e: Error) -> Self {
        Self::Account(e)
    }
}

/// Uses the supplied RPC client's commitment, timeouts and transport. No sends,
/// cached state, background polling, or automatic seed/proof retries.
///
/// ```no_run
/// use gacha_rust::{Buy, Client, RpcClient, RpcError};
/// use solana_pubkey::Pubkey;
/// # async fn example(url: String, pool: Pubkey, buyer: Pubkey, seed: [u8; 32]) -> Result<(), RpcError> {
/// let gacha = Client::new(RpcClient::new(url));
/// let purchase = gacha.buy(pool, Buy { buyer, count: 3, client_seed: seed }).await?;
/// // Your wallet signs/sends purchase.instruction. After confirmation:
/// let pull = gacha.fetch_pull(purchase.pull).await?;
/// # let _ = pull;
/// # Ok(())
/// # }
/// ```
pub struct Client {
    rpc: RpcClient,
}

impl Client {
    pub fn new(rpc: RpcClient) -> Self {
        Self { rpc }
    }

    pub async fn fetch_pool(&self, address: Pubkey) -> Result<Pool, RpcError> {
        let (_, accounts) = self.read(&[address], None, None).await?;
        Ok(Pool::from_account(address, accounts[0].0, &accounts[0].1)?)
    }
    pub async fn fetch_pull(&self, address: Pubkey) -> Result<Pull, RpcError> {
        let (_, accounts) = self.read(&[address], None, None).await?;
        Ok(Pull::from_account(address, accounts[0].0, &accounts[0].1)?)
    }
    pub async fn buy(&self, pool: Pubkey, buy: Buy) -> Result<Purchase, RpcError> {
        let (_, accounts) = self.read(&[pool], None, Some(POOL_HEADER_LEN)).await?;
        Ok(Pool::from_account(pool, accounts[0].0, &accounts[0].1)?.buy(buy)?)
    }
    /// Prepare return, payment and restock from one current pool/asset read.
    pub async fn buyback(
        &self,
        quote: &BuybackQuote,
        signature: &[u8; 64],
        payer: Pubkey,
    ) -> Result<Vec<Instruction>, RpcError> {
        let (_, accounts) = self
            .read(&[quote.pool, quote.asset], None, Some(POOL_HEADER_LEN))
            .await?;
        let pool = Pool::from_account(quote.pool, accounts[0].0, &accounts[0].1)?;
        let asset = Asset::from_account(quote.asset, accounts[1].0, &accounts[1].1)?;
        Ok(pool.buyback(quote, signature, &asset, payer)?)
    }

    pub async fn fetch_asset(&self, address: Pubkey) -> Result<Asset, RpcError> {
        Ok(self.assets(&[address], None).await?.remove(0))
    }

    pub async fn fetch_item(&self, address: Pubkey) -> Result<Item, RpcError> {
        let (_, accounts) = self.read(&[address], None, None).await?;
        Ok(Item::from_account(address, accounts[0].0, &accounts[0].1)?)
    }

    /// Prepare payment ATA creation and refund together; payer funds any rent.
    pub async fn refund(
        &self,
        address: Pubkey,
        payer: Pubkey,
    ) -> Result<Vec<Instruction>, RpcError> {
        let (pool, pull, _) = self.snapshot(address, false).await?;
        Ok(pool.refund(&pull, payer)?)
    }
    /// Resume the recorded outcomes, fetching ownership and collection headers.
    pub async fn deliver(
        &self,
        address: Pubkey,
        payer: Pubkey,
    ) -> Result<Vec<Vec<Instruction>>, RpcError> {
        let (slot, accounts) = self.read(&[address], None, None).await?;
        let pull = Pull::from_account(address, accounts[0].0, &accounts[0].1)?;
        let keys: Vec<_> = pull
            .outcomes()?
            .iter()
            .filter(|o| o.tier.is_some())
            .map(|o| o.asset)
            .collect();
        let assets = self.assets(&keys, Some(slot)).await?;
        Ok(pull.deliver(&assets, payer)?)
    }
    /// Fetch one coherent pool/pull snapshot, verify, then fetch exactly the
    /// selected items at least as recently. Returns ordered instruction groups.
    pub async fn settle(
        &self,
        address: Pubkey,
        proof: &Proof,
        payer: Pubkey,
    ) -> Result<Vec<Vec<Instruction>>, RpcError> {
        let (pool, pull, slot) = self.snapshot(address, true).await?;
        let plan = pool.settle(&pull, proof)?;
        let keys: Vec<_> = plan.draws().iter().map(|draw| draw.item).collect();
        let (slot, accounts) = self.read(&keys, Some(slot), None).await?;
        let items: Vec<_> = keys
            .iter()
            .zip(accounts)
            .map(|(key, (owner, data))| Item::from_account(*key, owner, &data))
            .collect::<Result<Vec<_>, _>>()?;
        let keys: Vec<_> = items.iter().map(Item::asset).collect();
        let assets = self.assets(&keys, Some(slot)).await?;
        Ok(plan.instructions(&items, &assets, payer)?)
    }
    async fn assets(&self, keys: &[Pubkey], minimum: Option<u64>) -> Result<Vec<Asset>, RpcError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let (_, accounts) = self.read(keys, minimum, Some(66)).await?;
        keys.iter()
            .zip(accounts)
            .map(|(key, (owner, data))| Ok(Asset::from_account(*key, owner, &data)?))
            .collect()
    }
    async fn snapshot(
        &self,
        address: Pubkey,
        inventory: bool,
    ) -> Result<(Pool, Pull, u64), RpcError> {
        let (slot, accounts) = self.read(&[address], None, None).await?;
        let discovered = Pull::from_account(address, accounts[0].0, &accounts[0].1)?;
        // One coherent read. For refunds, bound both slices to a complete Pull
        // and decode only the Pool header; historical inventory is irrelevant.
        let length = (!inventory).then_some(PULL_LEN);
        let (slot, accounts) = self
            .read(&[discovered.pool(), address], Some(slot), length)
            .await?;
        let pool_data = if inventory {
            &accounts[0].1[..]
        } else {
            accounts[0]
                .1
                .get(..POOL_HEADER_LEN)
                .ok_or(Error::InvalidAccount)?
        };
        let pool = Pool::from_account(discovered.pool(), accounts[0].0, pool_data)?;
        let pull = Pull::from_account(address, accounts[1].0, &accounts[1].1)?;
        Ok((pool, pull, slot))
    }
    async fn read(
        &self,
        keys: &[Pubkey],
        minimum: Option<u64>,
        length: Option<usize>,
    ) -> Result<(u64, Vec<(Pubkey, Vec<u8>)>), RpcError> {
        let response = self
            .rpc
            .get_multiple_ui_accounts_with_config(
                keys,
                RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    data_slice: length.map(|length| UiDataSliceConfig { offset: 0, length }),
                    commitment: Some(self.rpc.commitment()),
                    min_context_slot: minimum,
                },
            )
            .await?;
        if response.value.len() != keys.len()
            || minimum.is_some_and(|slot| response.context.slot < slot)
        {
            return Err(Error::InvalidAccount.into());
        }
        let accounts = keys
            .iter()
            .zip(response.value)
            .map(|(key, account)| {
                let account = account.ok_or(RpcError::MissingAccount(*key))?;
                let owner = account.owner.parse().map_err(|_| Error::InvalidAccount)?;
                let data = account.data.decode().ok_or(Error::InvalidAccount)?;
                Ok((owner, data))
            })
            .collect::<Result<Vec<_>, RpcError>>()?;
        Ok((response.context.slot, accounts))
    }
}
