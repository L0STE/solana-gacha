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
/// use gacha_program::client::{Buy, Client, RpcClient, RpcError};
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
    /// Requires the buyer's payment ATA; prepend native ATA CreateIdempotent if needed.
    pub async fn refund(&self, address: Pubkey) -> Result<Instruction, RpcError> {
        let (pool, pull, _) = self.snapshot(address, false).await?;
        Ok(pool.refund(&pull)?)
    }
    /// Resume delivery of the recorded outcomes, without a proof or inventory read.
    pub async fn deliver(
        &self,
        address: Pubkey,
        payer: Pubkey,
    ) -> Result<Vec<Vec<Instruction>>, RpcError> {
        Ok(self.fetch_pull(address).await?.deliver(payer)?)
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
        let (_, accounts) = self.read(&keys, Some(slot), None).await?;
        let items = keys
            .iter()
            .zip(accounts)
            .map(|(key, (owner, data))| Item::from_account(*key, owner, &data))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(plan.instructions(&items, payer)?)
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde_json::{json, Value};
    use solana_rpc_client::api::request::RpcRequest;

    fn data(a: &Value) -> Vec<u8> {
        a["data"]
            .as_str()
            .unwrap()
            .as_bytes()
            .chunks_exact(2)
            .map(|s| u8::from_str_radix(std::str::from_utf8(s).unwrap(), 16).unwrap())
            .collect()
    }
    fn key(a: &Value) -> Pubkey {
        a["address"].as_str().unwrap().parse().unwrap()
    }
    fn response(slot: u64, accounts: &[&Value]) -> Value {
        json!({"context": {"slot": slot}, "value": accounts.iter().map(|a| if a.is_null() { Value::Null } else {
            json!({"owner": a["owner"], "data": [STANDARD.encode(data(a)), "base64"],
                "lamports": 1, "executable": false, "rentEpoch": 0, "space": data(a).len()})
        }).collect::<Vec<_>>()})
    }
    fn client(responses: Vec<Value>) -> Client {
        Client::new(RpcClient::new_mock_with_mocks_map(
            "succeeds",
            responses
                .into_iter()
                .map(|r| (RpcRequest::GetMultipleAccounts, r))
                .collect(),
        ))
    }

    #[tokio::test]
    async fn rpc_composes_and_recovers_without_sending() {
        let v: Value = serde_json::from_str(include_str!("test-vector.json")).unwrap();
        let pool = key(&v["pool"]);
        let pull = key(&v["pull"]);
        let payer: Pubkey = v["operator"].as_str().unwrap().parse().unwrap();
        let buyer = v["buyer"].as_str().unwrap().parse().unwrap();
        let proof = Proof(data(&json!({"data": v["proof"]})).try_into().unwrap());
        let purchase = client(vec![response(10, &[&v["beforeBuy"]])])
            .buy(
                pool,
                Buy {
                    buyer,
                    count: 10,
                    client_seed: [7; 32],
                },
            )
            .await
            .unwrap();
        assert_eq!(purchase.pull, pull);
        let items: Vec<_> = v["items"].as_array().unwrap().iter().collect();
        let rpc = client(vec![
            response(10, &[&v["pull"]]),
            response(11, &[&v["pool"], &v["pull"]]),
            response(12, &items),
        ]);
        let groups = rpc.settle(pull, &proof, payer).await.unwrap();
        let pool = Pool::from_account(pool, PROGRAM_ID, &data(&v["pool"])).unwrap();
        let pending = Pull::from_account(pull, PROGRAM_ID, &data(&v["pull"])).unwrap();
        let items: Vec<_> = items
            .iter()
            .map(|a| Item::from_account(key(a), PROGRAM_ID, &data(a)).unwrap())
            .collect();
        assert_eq!(
            groups,
            pool.settle(&pending, &proof)
                .unwrap()
                .instructions(&items, payer)
                .unwrap()
        );
        // The RPC's shared data slice must retain the complete Pull. The Pool
        // response may end mid-inventory block; refunds decode only its header.
        let mut sliced_pool = v["pool"].clone();
        let mut pool_data = data(&sliced_pool);
        pool_data.resize(PULL_LEN, 0);
        sliced_pool["data"] = json!(pool_data
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>());
        let refund = client(vec![
            response(10, &[&v["pull"]]),
            response(11, &[&sliced_pool, &v["pull"]]),
        ])
        .refund(pull)
        .await
        .unwrap();
        assert_eq!(refund, pool.refund(&pending).unwrap());
        sliced_pool["data"] = json!("00".repeat(POOL_HEADER_LEN - 1));
        assert!(matches!(
            client(vec![
                response(10, &[&v["pull"]]),
                response(11, &[&sliced_pool, &v["pull"]]),
            ])
            .refund(pull)
            .await,
            Err(RpcError::Account(Error::InvalidAccount))
        ));
        let remaining = client(vec![response(20, &[&v["partialPull"]])])
            .deliver(pull, payer)
            .await
            .unwrap();
        assert_eq!(remaining, groups[1..]);
        let stale = client(vec![
            response(10, &[&v["pull"]]),
            response(9, &[&v["pool"], &v["pull"]]),
        ]);
        assert!(matches!(
            stale.settle(pull, &proof, payer).await,
            Err(RpcError::Account(Error::InvalidAccount))
        ));
        assert!(
            matches!(client(vec![response(10, &[&Value::Null])]).fetch_pull(pull).await, Err(RpcError::MissingAccount(a)) if a == pull)
        );
    }
}
