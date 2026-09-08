use base64::{engine::general_purpose::STANDARD, Engine};
use gacha_core::constants::PULL_LEN;
use gacha_rust::*;
use serde_json::{json, Value};
use solana_ecvrf::Proof;
use solana_pubkey::Pubkey;
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
    let v: Value = serde_json::from_str(include_str!("../fixtures/client.json")).unwrap();
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
    let item = client(vec![response(10, &[&v["reclaimItem"]])])
        .fetch_item(key(&v["reclaimItem"]))
        .await
        .unwrap();
    assert_eq!(item.address(), key(&v["reclaimItem"]));
    assert_eq!(item.asset(), key(&v["reclaimAsset"]));
    let quote = BuybackQuote {
        pool,
        asset: v["buybackQuote"]["asset"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
        price: v["buybackQuote"]["price"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
        expires_at: v["buybackQuote"]["expiresAt"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
        tier: v["buybackQuote"]["tier"].as_u64().unwrap() as u8,
    };
    let signature: [u8; 64] = data(&json!({"data": v["buybackSignature"]}))
        .try_into()
        .unwrap();
    let buyback = client(vec![response(10, &[&v["buybackPool"], &v["buybackAsset"]])])
        .buyback(&quote, &signature, payer)
        .await
        .unwrap();
    assert_eq!(
        buyback,
        Pool::from_account(pool, PROGRAM_ID, &data(&v["buybackPool"]))
            .unwrap()
            .buyback(
                &quote,
                &signature,
                &Asset::from_account(
                    key(&v["buybackAsset"]),
                    CORE_PROGRAM_ID,
                    &data(&v["buybackAsset"])
                )
                .unwrap(),
                payer
            )
            .unwrap()
    );
    let items: Vec<_> = v["items"].as_array().unwrap().iter().collect();
    let assets: Vec<_> = v["assets"].as_array().unwrap().iter().collect();
    let rpc = client(vec![
        response(10, &[&v["pull"]]),
        response(11, &[&v["pool"], &v["pull"]]),
        response(12, &items),
        response(13, &assets),
    ]);
    let groups = rpc.settle(pull, &proof, payer).await.unwrap();
    let pool = Pool::from_account(pool, PROGRAM_ID, &data(&v["pool"])).unwrap();
    let pending = Pull::from_account(pull, PROGRAM_ID, &data(&v["pull"])).unwrap();
    let items: Vec<_> = items
        .iter()
        .map(|a| Item::from_account(key(a), PROGRAM_ID, &data(a)).unwrap())
        .collect();
    let assets: Vec<_> = assets
        .iter()
        .map(|a| Asset::from_account(key(a), CORE_PROGRAM_ID, &data(a)).unwrap())
        .collect();
    assert_eq!(
        groups,
        pool.settle(&pending, &proof)
            .unwrap()
            .instructions(&items, &assets, payer)
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
    .refund(pull, payer)
    .await
    .unwrap();
    assert_eq!(refund, pool.refund(&pending, payer).unwrap());
    sliced_pool["data"] = json!("00".repeat(POOL_HEADER_LEN - 1));
    assert!(matches!(
        client(vec![
            response(10, &[&v["pull"]]),
            response(11, &[&sliced_pool, &v["pull"]]),
        ])
        .refund(pull, payer)
        .await,
        Err(RpcError::Account(Error::InvalidAccount))
    ));
    let recovery_assets: Vec<_> = v["recoveryAssets"].as_array().unwrap().iter().collect();
    let remaining = client(vec![
        response(20, &[&v["partialPull"]]),
        response(21, &recovery_assets),
    ])
    .deliver(pull, payer)
    .await
    .unwrap();
    let recovery_assets: Vec<_> = recovery_assets
        .iter()
        .map(|a| Asset::from_account(key(a), CORE_PROGRAM_ID, &data(a)).unwrap())
        .collect();
    assert_eq!(
        remaining,
        Pull::from_account(pull, PROGRAM_ID, &data(&v["partialPull"]))
            .unwrap()
            .deliver(&recovery_assets, payer)
            .unwrap()
    );
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
