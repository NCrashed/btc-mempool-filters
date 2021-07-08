use bitcoin::Transaction;
use bitcoin::Txid;
use bitcoin_utxo::cache::utxo::UtxoCache;
use bitcoin_utxo::cache::utxo::get_utxo_noh;
use bitcoin::consensus::Decodable;
use bitcoin::consensus::encode;
use bitcoin::network::message_blockdata::Inventory;
use bitcoin::network::message::NetworkMessage;
use bitcoin::OutPoint;
use bitcoin::Script;
use bitcoin::util::bip158;
use ergvein_filters::mempool::ErgveinMempoolFilter;
use futures::Future;

use futures::future::AbortHandle;

use futures::future::Abortable;use futures::sink;
use rocksdb::DB;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use std::time::Duration;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

use crate::error::MempoolErrors;
use crate::filtertree::FilterTree;
use crate::filtertree::make_filters;
use crate::filtertree::make_full_filter;

use crate::txtree::get_transaction_script;
use crate::txtree::insert_tx;
use crate::txtree::TxTree;
use crate::txtree::remove_batch;
use crate::txtree::tx_tree_count;

pub async fn mempool_worker<T, M>(
    txtree: Arc<TxTree>,
    ftree: Arc<FilterTree>,
    full_filter: Arc<Mutex<Option<ErgveinMempoolFilter>>>,
    db: Arc<DB>,
    cache: Arc<UtxoCache<T>>,
    sync_mutex: Arc<Mutex<()>>,
    script_from_t : M,
    filter_delay: Duration
) -> (
    impl Future<Output = Result<(), MempoolErrors>>,
    impl Future<Output = Result<(), MempoolErrors>>,
    impl futures::Sink<NetworkMessage, Error = encode::Error>,
    impl Unpin + futures::Stream<Item = NetworkMessage>
)
where
T:Decodable + Clone,
M:Fn(&T) -> Script + Copy
{
    const BUFFER_SIZE: usize = 100;
    let (broad_sender, _) = broadcast::channel(BUFFER_SIZE);
    let (msg_sender, msg_reciver) = mpsc::unbounded_channel::<NetworkMessage>();
    let tx_future = {
        let broad_sender = broad_sender.clone();
        let msg_sender = msg_sender.clone();
        let txtree = txtree.clone();
        async move {
            loop {
                tx_listener(txtree.clone(), &broad_sender, &msg_sender).await?;
            }
        }
    };
    let filt_future = {
        let broad_sender = broad_sender.clone();
        let msg_sender = msg_sender.clone();
        async move {
            filter_worker(
                txtree.clone(),
                &broad_sender,
                &msg_sender,
                ftree.clone(),
                full_filter.clone(),
                db.clone(),
                cache.clone(),
                sync_mutex.clone(),
                script_from_t,
                filter_delay
            ).await
        }
    };

    let msg_stream = UnboundedReceiverStream::new(msg_reciver);
    let msg_sink = sink::unfold(broad_sender, |broad_sender, msg| async move {
        broad_sender.send(msg).unwrap_or(0);
        Ok::<_, encode::Error>(broad_sender)
    });
    (tx_future, filt_future, msg_sink, msg_stream)
}

pub async fn filter_worker<T, M>(
    txtree: Arc<TxTree>,
    broad_sender: &broadcast::Sender<NetworkMessage>,
    msg_sender: &mpsc::UnboundedSender<NetworkMessage>,
    ftree: Arc<FilterTree>,
    full_filter: Arc<Mutex<Option<ErgveinMempoolFilter>>>,
    db: Arc<DB>,
    cache: Arc<UtxoCache<T>>,
    sync_mutex: Arc<Mutex<()>>,
    script_from_t : M,
    filter_delay: Duration
) -> Result<(), MempoolErrors>
where
T:Decodable + Clone,
M:Fn(&T) -> Script + Copy
{
    println!("[filter_worker]: Starting");
    let mut hashmap = HashMap::<OutPoint, Script>::new();
    let mut timeout = 10;
    let mut err_cnt = 0;
    let mut succ_cnt = 0;
    loop {
        tokio::time::sleep(filter_delay).await;
        { // Make sure the utxo cache is fully synced before attempting to make filters
            let _guard = sync_mutex.lock().await;
            hashmap.clear();
            let timeout_future = tokio::time::sleep(Duration::from_secs(timeout));
            let new_block_future = wait_for_block(&broad_sender);
            let (timeout_handle, timeout_reg) = AbortHandle::new_pair();
            tokio::pin!(timeout_future);
            let fill_fut = Abortable::new(
                fill_tx_map(
                    txtree.clone(),
                    db.clone(),
                    cache.clone(),
                    &mut hashmap,
                    broad_sender,
                    msg_sender,
                    script_from_t)
                ,timeout_reg);
            tokio::select!{
                _ = &mut timeout_future => {
                    println!("Hashmap filling timed out");
                    timeout_handle.abort();
                    // increase timeout for the next try. Just in case the default timeout was not enough
                    timeout += timeout.wrapping_div(5);
                }
                _ = new_block_future => {
                    println!("A new block interrupted filter making. Aborting filters!");
                    timeout_handle.abort();
                }
                res = fill_fut => {
                    match res {
                        Ok(_) => println!("[filter_worker]: Fill success. Attempting to make filters"),
                        Err(e) => eprint!("Failed to fill inputs! {:?}", e)
                    }
                    timeout = 10;
                }
            };

        } // Unlock sync_mutex, sine we don't need the cache anymore
        make_filters(&ftree, &txtree, |out| {
            hashmap.get(out).map_or( {
                let e = Err(bip158::Error::UtxoMissing(*out));
                eprintln!("Failed to extract the script for: {:?}", e);
                e
            }, |s| Ok(s.clone()))

        });

        let ff = make_full_filter(&txtree, |out| {
            hashmap.get(out).map_or( Err(bip158::Error::UtxoMissing(*out)), |s| Ok(s.clone()))
        }).map_err(|e| eprintln!("Error making the full filter! {:?}", e)).ok();
        {
            let mut ffref = full_filter.lock().await;
            *ffref = ff;
            let l = ffref.as_ref().map(|f| f.content.len());
            if ffref.is_none() { err_cnt += 1; } else { succ_cnt +=1; }
            println!("Filter len: {:?}. Transactions in mempool: {}. Succ: {}. Error: {}", l, tx_tree_count(&txtree), succ_cnt, err_cnt);
        }
        println!("[filter_worker]: Filters done");
    };
}

pub async fn tx_listener(
    txtree: Arc<TxTree>,
    broad_sender: &broadcast::Sender<NetworkMessage>,
    msg_sender: &mpsc::UnboundedSender<NetworkMessage>,
) -> Result<(), MempoolErrors>{
    let mut receiver = broad_sender.subscribe();
    loop {
        tokio::select! {
            emsg = receiver.recv() => match emsg {
                Ok(msg) => match msg {
                    NetworkMessage::Inv(ids) => {
                        let txids : Vec<Inventory>= ids.iter().filter(|i| match i {
                            Inventory::Transaction(_) => true,
                            Inventory::WitnessTransaction(_) => true,
                            _ => false
                        }).cloned().collect();
                        // println!("Received {:?} invs. Of which {:?} are txs", ids.len(), txids.len());
                        msg_sender.send(NetworkMessage::GetData(txids))
                            .map_err(|e| { println!("Error when requesting txs: {:?}", e); MempoolErrors::MempoolRequestFail})?;
                    },
                    NetworkMessage::Tx(tx) => {
                        let txtree = txtree.clone();
                        insert_tx(&txtree, &tx);
                    }
                    NetworkMessage::Block(b) => {
                        let txtree = txtree.clone();
                        remove_batch(&txtree, &b.txdata);
                    }
                    _ => ()
                },
                Err(e) => eprintln!("tx_listener: {:?}", e),
            }
        }
    };
}

pub async fn request_mempool_tx(
    txid: Txid,
    broad_sender: &broadcast::Sender<NetworkMessage>,
    msg_sender: &mpsc::UnboundedSender<NetworkMessage>,
) -> Result<Transaction, MempoolErrors>{
    let mut receiver = broad_sender.subscribe();
    println!("Requesting from node. Tx: {:?}", txid);
    msg_sender.send(NetworkMessage::GetData(vec![Inventory::Transaction(txid)])).map_err(
        |_| MempoolErrors::MempoolRequestFail
    )?;
    let timeout_future = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(timeout_future);
    let mut res = None;
    while res == None {
        tokio::select! {
            _ = &mut timeout_future => {
                println!("Request for Tx: {} timed out", txid);
                break;
            }
            emsg = receiver.recv() => match emsg {
                Err(e) => {
                    eprintln!("Request tx {:?} failed with recv error: {:?}", txid, e);
                    msg_sender.send(NetworkMessage::GetData(vec![Inventory::Transaction(txid)])).map_err(
                        |_| MempoolErrors::MempoolRequestFail
                    )?;
                }
                Ok(msg) => match msg {
                    NetworkMessage::Tx(tx) if tx.txid() == txid => {
                        res = Some(tx);
                    }
                    _ => (),
                },
            }
        }
    };
    res.ok_or(MempoolErrors::MempoolRequestFail)
}

async fn fill_tx_map<T,M>(
    txtree: Arc<TxTree>,
    db: Arc<DB>,
    cache: Arc<UtxoCache<T>>,
    hashmap: &mut HashMap<OutPoint, Script>,
    broad_sender: &broadcast::Sender<NetworkMessage>,
    msg_sender: &mpsc::UnboundedSender<NetworkMessage>,
    script_from_t : M,
)
where
T:Decodable + Clone,
M:Fn(&T) -> Script + Copy
{
    let mut extra = Vec::new();
    for txs in txtree.iter(){
        for tx in txs.values(){
            if !tx.is_coin_base(){
                for i in tx.input.iter(){
                    let stx = fill_tx_input(
                        &i.previous_output,
                        txtree.clone(),
                        db.clone(),
                        cache.clone(),
                        hashmap,
                        broad_sender,
                        msg_sender,
                        script_from_t,
                    ).await;
                    stx.map(|tx| extra.push(tx));
                };
            }
        }
    }
    while !extra.is_empty() {
        let mut next_extra = Vec::new();
        for tx in extra.iter(){
            for i in tx.input.iter(){
                let stx = fill_tx_input(
                    &i.previous_output,
                    txtree.clone(),
                    db.clone(),
                    cache.clone(),
                    hashmap,
                    broad_sender,
                    msg_sender,
                    script_from_t,
                ).await;
                stx.map(|tx| next_extra.push(tx));
            }
        }
        extra = next_extra;
    }
}

async fn fill_tx_input<T, M>(
    i: &OutPoint,
    txtree: Arc<TxTree>,
    db: Arc<DB>,
    cache: Arc<UtxoCache<T>>,
    hashmap: &mut HashMap<OutPoint, Script>,
    broad_sender: &broadcast::Sender<NetworkMessage>,
    msg_sender: &mpsc::UnboundedSender<NetworkMessage>,
    script_from_t : M,
) -> Option<Transaction>
where
T:Decodable + Clone,
M:Fn(&T) -> Script + Copy
{
    let mut res = None;
    let coin = get_utxo_noh(
        &db.clone(),
        &cache.clone(),
        &i);
    match coin {
        Some(c) => {
            hashmap.insert(i.clone(), script_from_t(&c));
        }
        None => {
            let mscript = get_transaction_script(&txtree, i);
            match mscript{
                Some(script) => {hashmap.insert(i.clone(), script);},
                None => {
                    let stx = request_mempool_tx(i.txid, broad_sender, msg_sender).await;
                    match stx {
                        Ok(tx) => {
                            let script = tx.output[i.vout as usize].script_pubkey.clone();
                            hashmap.insert(i.clone(), script);
                            res = Some(tx);
                        },
                        Err(_) => {
                            eprintln!("Failed to get Tx {:?} from mempool", i.txid);
                        },
                    }
                }
            }
        }
    }
    res
}

async fn wait_for_block(
    broad_sender: &broadcast::Sender<NetworkMessage>,
){
    let mut receiver = broad_sender.subscribe();
    loop {
        let emsg = receiver.recv().await;
        match emsg {
            Err(_) => {},
            Ok(msg) => match msg{
                NetworkMessage::Block(_) => break,
                _ => ()
            }

        }
    }
}
