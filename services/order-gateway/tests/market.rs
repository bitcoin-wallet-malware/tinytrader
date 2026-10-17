//! Market mode: synthetic `cmbp-1` and definition records served as a DBN stream (the
//! `replay dbn` wire), client-clocked with `advance`. Determinism, touch arithmetic,
//! resting fills, day expiry and the five ordering rules.
mod support;
use dbn::encode::{DbnEncodable, DbnMetadataEncoder, DbnRecordEncoder, EncodeRecord};
use dbn::record::str_to_c_chars;
use dbn::{Cmbp1Msg, InstrumentDefMsg, Metadata, RecordHeader, SType, Schema, rtype};
use order_gateway::{SimBroker, start};
use serde_json::Value;
use std::io::Write;
use std::net::TcpListener;
use support::*;

const MS: u64 = 1_000_000;
/// 2026-10-05 16:00 `America/New_York`.
const CLOSE: u64 = 1_791_230_400_000_000_000;

/// One record as DBN bytes, encoded by Databento's `dbn` crate.
fn bytes<R: DbnEncodable>(r: &R) -> Vec<u8> {
    let mut out = Vec::new();
    DbnRecordEncoder::new(&mut out).encode_record(r).unwrap();
    out
}

fn def(iid: u32, ts: u64, symbol: &str) -> Vec<u8> {
    // OPRA raw symbols pad the root to six characters.
    let padded = format!("{:<6}{}", &symbol[..3], &symbol[3..]);
    bytes(&InstrumentDefMsg {
        hd: RecordHeader::new::<InstrumentDefMsg>(rtype::INSTRUMENT_DEF, 1, iid, ts),
        ts_recv: ts,
        raw_symbol: str_to_c_chars(&padded).unwrap(),
        ..Default::default()
    })
}

/// `cents` of `None` is an absent side.
fn quote(iid: u32, ts: u64, bid: Option<i64>, ask: Option<i64>, size: u32) -> Vec<u8> {
    let px = |c: Option<i64>| c.map_or(dbn::UNDEF_PRICE, |c| c * 10_000_000);
    let mut q = Cmbp1Msg {
        hd: RecordHeader::new::<Cmbp1Msg>(rtype::CMBP_1, 1, iid, ts),
        ts_recv: ts,
        ..Cmbp1Msg::default_for_schema(Schema::Cmbp1)
    };
    q.levels[0].bid_px = px(bid);
    q.levels[0].ask_px = px(ask);
    q.levels[0].bid_sz = size;
    q.levels[0].ask_sz = size;
    bytes(&q)
}

/// A DBN stream: header, the three definitions at `T0 - 1 s`, then `quotes`.
fn stream(quotes: &[Vec<u8>]) -> Vec<u8> {
    let meta = Metadata::builder()
        .dataset("OPRA.PILLAR")
        .schema(Some(Schema::Cmbp1))
        .start(T0)
        .stype_in(Some(SType::RawSymbol))
        .stype_out(SType::InstrumentId)
        .build();
    let mut out = Vec::new();
    DbnMetadataEncoder::new(&mut out).encode(&meta).unwrap();
    for (iid, sym) in [(1, CALL), (2, PUT_A), (3, PUT_B)] {
        out.extend(def(iid, T0 - 1_000 * MS, sym));
    }
    for q in quotes {
        out.extend(q);
    }
    out
}

/// Serves the bytes once over loopback, as `replay dbn --listen` does.
fn serve(bytes: Vec<u8>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let _ = s.write_all(&bytes);
    });
    format!("tcp:{addr}")
}

fn run(name: &str, script: &str, quotes: &[Vec<u8>], body: impl FnOnce(&mut Conn)) -> Vec<String> {
    let sim = SimBroker::market(script, &serve(stream(quotes))).unwrap();
    let gw = start(config(root(name)), sim).unwrap();
    let mut c = Conn::open(&gw, "reactor", None);
    body(&mut c);
    gw.stop().unwrap();
    c.lines[1..].to_vec()
}

fn advance(c: &mut Conn, to: u64) -> Vec<Value> {
    let rid = format!("adv-{to}");
    let (r, events) = c.req("advance", &rid, &format!(r#""to_ts":{to}"#));
    assert_eq!(r["market_ts"], to);
    events
}

fn fill_price(events: &[Value], cid: &str) -> Option<String> {
    events
        .iter()
        .find(|v| v["kind"] == "order" && v["client_order_id"] == cid && v["fill"].is_object())
        .map(|v| v["fill"]["price"].as_str().unwrap().to_string())
}

fn main_session(c: &mut Conn) {
    advance(c, T0);
    // Resting single: ask 1.30 above the 1.25 limit until a later quote improves it.
    assert_eq!(c.submit("r1", &buy("wse-rest"))["outcome"], "accepted");
    let ev = advance(c, T0 + 10 * MS);
    assert_eq!(fill_price(&ev, "wse-rest"), None);
    let ev = advance(c, T0 + 30 * MS);
    assert_eq!(fill_price(&ev, "wse-rest").as_deref(), Some("1.24"));
    // Credit spread: N = 0.52 - 0.93 = -0.41 <= -0.40, atomic, legs at their touches.
    c.submit("r2", &credit_spread("wse-cs", 2));
    let ev = advance(c, T0 + 40 * MS);
    let f = ev
        .iter()
        .find(|v| v["client_order_id"] == "wse-cs" && v["event"] == "fill")
        .unwrap();
    assert_eq!(
        (f["fill"]["price"].as_str(), f["filled_qty"].as_u64()),
        (Some("-0.41"), Some(2))
    );
    assert_eq!(
        (
            f["legs"][0]["avg_price"].as_str(),
            f["legs"][1]["avg_price"].as_str()
        ),
        (Some("0.93"), Some("0.52"))
    );
    // A day order that never fills expires at the session close.
    c.submit("r3", &single("wse-day", CALL, "buy", "open", 1, "1.00"));
    let ev = advance(c, CLOSE + 1_000 * MS);
    let e = ev
        .iter()
        .find(|v| v["client_order_id"] == "wse-day" && v["event"] == "expired")
        .unwrap();
    assert_eq!(e["ts_broker"], CLOSE);
    let r = c.submit("r4", &single("wse-late", CALL, "sell", "close", 1, "1.00"));
    assert_eq!(r["code"], "sim_market_closed");
}

fn main_quotes() -> Vec<Vec<u8>> {
    vec![
        quote(1, T0, Some(120), Some(130), 10),
        quote(2, T0, Some(93), Some(95), 10),
        quote(3, T0, Some(50), Some(52), 10),
        quote(1, T0 + 20 * MS, Some(120), Some(124), 10),
    ]
}

#[test]
fn market_session_is_byte_identical_across_runs() {
    let a = run("market-a", "{}", &main_quotes(), main_session);
    let b = run("market-b", "{}", &main_quotes(), main_session);
    assert_eq!(a, b);
    assert!(a.iter().any(|l| l.contains(r#""source":"sim""#)));
}

#[test]
fn dbn_file_source_and_unknown_instrument() {
    let path = root("market-file").join("quotes.dbn");
    std::fs::write(&path, stream(&main_quotes())).unwrap();
    let sim = SimBroker::market("{}", path.to_str().unwrap()).unwrap();
    let gw = start(config(root("market-file-state")), sim).unwrap();
    let mut c = Conn::open(&gw, "reactor", None);
    advance(&mut c, T0);
    let r = c.submit(
        "r1",
        &single("wse-x", "SPY261005C00590000", "buy", "open", 1, "1.00"),
    );
    assert_eq!(
        (r["outcome"].as_str(), r["code"].as_str()),
        (Some("rejected"), Some("sim_unknown_instrument"))
    );
    gw.stop().unwrap();
}

/// A market close takes the touch, whatever its reference price: a sell the bid.
/// A market opening is refused, so the position is opened at a limit.
#[test]
fn market_orders_fill_at_the_touch() {
    let q = vec![quote(1, T0, Some(190), Some(200), 5)];
    let market = |s: String| {
        s.replace(
            r#""type":"limit","limit_price""#,
            r#""type":"market","ref_price""#,
        )
    };
    run("market-type", "{}", &q, |c| {
        advance(c, T0);
        let r = c.submit("r1", &single("wse-m1", CALL, "buy", "open", 1, "2.00"));
        assert_eq!(r["outcome"], "accepted", "{r}");
        let ev = advance(c, T0 + 10 * MS);
        assert_eq!(fill_price(&ev, "wse-m1").as_deref(), Some("2.00"));
        // The close waits for the buy on its leg to fill, so it is sent after it.
        let r = c.submit(
            "r2",
            &market(single("wse-m2", CALL, "sell", "close", 1, "3.00")),
        );
        assert_eq!(r["outcome"], "accepted", "{r}");
        let ev = advance(c, T0 + 20 * MS);
        assert_eq!(fill_price(&ev, "wse-m2").as_deref(), Some("1.90"));
    });
}

/// Rule 1: records with equal timestamps apply in input order.
#[test]
fn rule1_equal_timestamps_keep_input_order() {
    let t1 = T0 + 50 * MS;
    let q = vec![
        quote(1, T0, Some(190), Some(200), 5),
        quote(1, t1, Some(90), Some(100), 5),
        quote(1, t1, Some(110), Some(120), 5),
    ];
    run("rule1", "{}", &q, |c| {
        advance(c, T0);
        c.submit("r1", &single("wse-1", CALL, "buy", "open", 1, "1.50"));
        let ev = advance(c, t1);
        assert_eq!(fill_price(&ev, "wse-1").as_deref(), Some("1.00"));
    });
}

/// Rule 2: an action at t follows every record with ts <= t.
#[test]
fn rule2_activation_follows_quotes_at_its_time() {
    let q = vec![
        quote(1, T0, Some(110), Some(120), 5),
        quote(1, T0 + 5 * MS, Some(100), Some(110), 5),
    ];
    run("rule2", "{}", &q, |c| {
        advance(c, T0);
        c.submit("r1", &buy("wse-1"));
        let ev = advance(c, T0 + 10 * MS);
        assert_eq!(fill_price(&ev, "wse-1").as_deref(), Some("1.10"));
    });
}

/// Rule 3: activation sees the book at `t_act`; only quotes on a leg re-evaluate; a
/// crossed quote makes the instrument unusable until a non-crossed one.
#[test]
fn rule3_reevaluation_on_touching_quotes_only() {
    let q = vec![
        quote(1, T0, Some(120), Some(130), 5),
        quote(2, T0 + 20 * MS, Some(10), Some(11), 5),
        quote(1, T0 + 30 * MS, Some(130), Some(125), 5),
        quote(1, T0 + 40 * MS, Some(120), Some(125), 5),
    ];
    run("rule3", "{}", &q, |c| {
        advance(c, T0);
        c.submit("r1", &buy("wse-1"));
        assert_eq!(
            fill_price(&advance(c, T0 + 35 * MS), "wse-1"),
            None,
            "unrelated or crossed"
        );
        assert_eq!(
            fill_price(&advance(c, T0 + 45 * MS), "wse-1").as_deref(),
            Some("1.25")
        );
    });
}

/// Rule 4: orders fillable on one record go in (`t_act`, `seq`) order, sharing displayed size.
#[test]
fn rule4_priority_and_displayed_size() {
    let q = vec![
        quote(1, T0, Some(120), Some(130), 5),
        quote(1, T0 + 50 * MS, Some(110), Some(120), 1),
        quote(1, T0 + 60 * MS, Some(110), Some(120), 1),
    ];
    run("rule4", r#"{"size_cap":"displayed"}"#, &q, |c| {
        advance(c, T0);
        c.submit("r1", &buy("wse-x"));
        c.submit("r2", &single("wse-y", CALL, "buy", "open", 1, "1.29"));
        let ev = advance(c, T0 + 55 * MS);
        assert_eq!(fill_price(&ev, "wse-x").as_deref(), Some("1.20"));
        assert_eq!(
            fill_price(&ev, "wse-y"),
            None,
            "the displayed size went to the earlier order"
        );
        assert_eq!(
            fill_price(&advance(c, T0 + 65 * MS), "wse-y").as_deref(),
            Some("1.20")
        );
    });
    run("rule4-atomic", "{}", &q, |c| {
        advance(c, T0);
        c.submit("r1", &buy("wse-x"));
        c.submit("r2", &single("wse-y", CALL, "buy", "open", 1, "1.29"));
        let ev = advance(c, T0 + 55 * MS);
        let order: Vec<&str> = ev
            .iter()
            .filter(|v| v["event"] == "fill")
            .map(|v| v["client_order_id"].as_str().unwrap())
            .collect();
        assert_eq!(order, ["wse-x", "wse-y"]);
    });
}

/// Rule 5: a fill and a cancel effect at the same time: the fill wins.
#[test]
fn rule5_fill_beats_simultaneous_cancel() {
    let t = T0 + 20 * MS;
    let q = vec![
        quote(1, T0, Some(120), Some(130), 5),
        quote(1, t + 5 * MS, Some(110), Some(120), 5),
    ];
    run("rule5", "{}", &q, |c| {
        advance(c, T0);
        c.submit("r1", &buy("wse-1"));
        advance(c, t);
        let (r, _) = c.req("cancel", "c1", r#""client_order_id":"wse-1""#);
        assert_eq!(r["outcome"], "accepted");
        let ev = advance(c, t + 10 * MS);
        assert_eq!(fill_price(&ev, "wse-1").as_deref(), Some("1.20"));
        assert!(!ev.iter().any(|v| v["event"] == "canceled"));
    });
    let q = vec![quote(1, T0, Some(120), Some(130), 5)];
    run("rule5-cancel", "{}", &q, |c| {
        advance(c, T0);
        c.submit("r1", &buy("wse-1"));
        c.req("cancel", "c1", r#""client_order_id":"wse-1""#);
        let ev = advance(c, T0 + 10 * MS);
        let e = ev.iter().find(|v| v["event"] == "canceled").unwrap();
        assert_eq!(
            e["ts_broker"],
            T0 + 5 * MS,
            "the cancel takes effect after its latency"
        );
    });
}
