//! The v1 order intent: protocol validation, canonical form and `intent_sha256`.
use crate::protocol::{Fields, Invalid, Obj, arr, invalid};
use crate::safety::days_from_civil;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// One leg; a single order is one leg with ratio 1.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Leg {
    /// Compact OCC symbol, or a stock's ticker when `equity`.
    pub symbol: String,
    /// `class: equity` (shares; single orders only); otherwise `option`.
    pub equity: bool,
    /// Ratio per parent unit (1 for a single).
    pub ratio: u32,
    /// `side: buy`.
    pub buy: bool,
    /// `effect: open`.
    pub open: bool,
}

/// A validated day intent of one allowlisted shape: a limit order, or a
/// market order carrying the client's reference price for the caps.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Intent {
    /// The only logical order key.
    pub client_order_id: String,
    /// `shape: combo`.
    pub combo: bool,
    /// One leg for a single, 2 to 4 for a combo.
    pub legs: Vec<Leg>,
    /// Contracts (parent units for a combo).
    pub qty: u32,
    /// Limit in cents; a combo is signed (+ debit, − credit). For a market
    /// order, its `ref_price`: what the client expects it to cost (the
    /// natural price when sent), which the notional cap is checked against.
    /// It doesn't bound the fill.
    pub limit_cents: i64,
    /// `type: market`: sent with no price; fills at whatever the market gives.
    pub market: bool,
    /// `time_in_force: cls`: the closing auction (limit-on-close), stock singles only.
    pub on_close: bool,
}

pub(crate) fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// Root and expiry (days since the epoch) of a compact OCC symbol.
pub(crate) fn occ(symbol: &str) -> Option<(&str, i64)> {
    let b = symbol.as_bytes();
    let root = b.len().checked_sub(15).filter(|n| (1..=6).contains(n))?;
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if !b[..root].iter().all(u8::is_ascii_uppercase)
        || !digits(root..root + 6)
        || !matches!(b[root + 6], b'C' | b'P')
        || !digits(root + 7..b.len())
    {
        return None;
    }
    let num = |i: usize| i64::from(b[i] - b'0') * 10 + i64::from(b[i + 1] - b'0');
    let (y, m, d) = (2000 + num(root), num(root + 2), num(root + 4));
    let days = days_from_civil(y, u32::try_from(m).ok()?, u32::try_from(d).ok()?);
    let back = crate::safety::civil_from_days(days);
    (back == (y, u32::try_from(m).ok()?, u32::try_from(d).ok()?)).then_some((&symbol[..root], days))
}

/// The parts of an option that its maximum loss needs.
pub(crate) struct Contract<'a> {
    pub root: &'a str,
    /// Days since the epoch.
    pub expiry: i64,
    pub put: bool,
    /// Whole cents; `None` when the strike has a fraction of a cent.
    pub strike_cents: Option<i64>,
}

/// The contract of a valid compact OCC symbol.
pub(crate) fn contract(symbol: &str) -> Option<Contract<'_>> {
    let (root, expiry) = occ(symbol)?;
    let mills: i64 = symbol[root.len() + 7..].parse().ok()?;
    Some(Contract {
        root,
        expiry,
        put: symbol.as_bytes()[root.len() + 6] == b'P',
        strike_cents: (mills % 10 == 0).then_some(mills / 10),
    })
}

/// Parses a decimal price with at most two decimal places into cents.
pub(crate) fn parse_cents(s: &str) -> Option<i64> {
    let (neg, body) = s.strip_prefix('-').map_or((false, s), |b| (true, b));
    let (int, frac) = body.split_once('.').unwrap_or((body, "00"));
    let ok = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if int.is_empty() || int.len() > 9 || frac.is_empty() || frac.len() > 2 || !ok(int) || !ok(frac)
    {
        return None;
    }
    let cents = int.parse::<i64>().ok()? * 100
        + frac.parse::<i64>().ok()? * if frac.len() == 1 { 10 } else { 1 };
    Some(if neg { -cents } else { cents })
}

/// Formats cents with exactly two decimal places.
pub(crate) fn fmt_cents(c: i64) -> String {
    let sign = if c < 0 { "-" } else { "" };
    let a = c.unsigned_abs();
    format!("{sign}{}.{:02}", a / 100, a % 100)
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// A stock ticker: 1 to 5 capitals, with an optional class suffix (`BRK.B`).
pub(crate) fn ticker(symbol: &str) -> bool {
    let (base, class) = symbol.split_once('.').unwrap_or((symbol, "A"));
    (1..=5).contains(&base.len())
        && base.bytes().all(|b| b.is_ascii_uppercase())
        && class.len() == 1
        && class.bytes().all(|b| b.is_ascii_uppercase())
}

/// `(symbol, equity)`.
fn instrument(f: &mut Fields) -> Result<(String, bool), Invalid> {
    let mut i = Fields::new(f.req("instrument")?, &f.at("instrument"))?;
    let equity = match i.string("class")?.as_str() {
        "option" => false,
        "equity" => true,
        _ => return Err(invalid(&i.at("class"), "option or equity")),
    };
    let symbol = i.string("symbol")?;
    if equity && !ticker(&symbol) {
        return Err(invalid(
            &i.at("symbol"),
            "a stock ticker such as AAPL or BRK.B",
        ));
    }
    if !equity && occ(&symbol).is_none() {
        return Err(invalid(
            &i.at("symbol"),
            "compact OCC symbol with a valid date",
        ));
    }
    i.finish()?;
    Ok((symbol, equity))
}

fn side_effect(f: &mut Fields) -> Result<(bool, bool), Invalid> {
    let buy = match f.string("side")?.as_str() {
        "buy" => true,
        "sell" => false,
        _ => return Err(invalid(&f.at("side"), "buy or sell")),
    };
    let open = match f.string("effect")?.as_str() {
        "open" => true,
        "close" => false,
        _ => return Err(invalid(&f.at("effect"), "open or close")),
    };
    Ok((buy, open))
}

impl Intent {
    /// Protocol validation (section 6); the path prefix is `intent`.
    pub(crate) fn parse(v: Value) -> Result<Self, Invalid> {
        let mut f = Fields::new(v, "intent")?;
        let client_order_id = f.string("client_order_id")?;
        if !valid_id(&client_order_id) {
            return Err(invalid(
                &f.at("client_order_id"),
                "1 to 128 of [A-Za-z0-9._:-]",
            ));
        }
        let combo = match f.string("shape")?.as_str() {
            "single" => false,
            "combo" => true,
            _ => return Err(invalid(&f.at("shape"), "single or combo")),
        };
        let legs = if combo {
            let path = f.at("legs");
            let raw = f.array("legs")?;
            if !(2..=4).contains(&raw.len()) {
                return Err(invalid(&path, "2 to 4 legs"));
            }
            let mut legs = Vec::new();
            for (n, v) in raw.into_iter().enumerate() {
                let mut l = Fields::new(v, &format!("{path}[{n}]"))?;
                let (symbol, equity) = instrument(&mut l)?;
                if equity {
                    return Err(invalid(&format!("{path}[{n}]"), "combos are options only"));
                }
                let ratio = u32::try_from(l.int("ratio", 1, 10_000)?).unwrap_or(1);
                let (buy, open) = side_effect(&mut l)?;
                l.finish()?;
                if legs.iter().any(|x: &Leg| x.symbol == symbol) {
                    return Err(invalid(&path, "leg symbols must be distinct"));
                }
                legs.push(Leg {
                    symbol,
                    equity,
                    ratio,
                    buy,
                    open,
                });
            }
            if legs.iter().fold(0, |g, l| gcd(g, l.ratio)) != 1 {
                return Err(invalid(&path, "leg ratios must have gcd 1"));
            }
            legs
        } else {
            let (symbol, equity) = instrument(&mut f)?;
            let (buy, open) = side_effect(&mut f)?;
            vec![Leg {
                symbol,
                equity,
                ratio: 1,
                buy,
                open,
            }]
        };
        let qty = u32::try_from(f.int("qty", 1, 1_000_000)?).unwrap_or(1);
        let market = match f.string("type")?.as_str() {
            "limit" => false,
            "market" => true,
            _ => return Err(invalid(&f.at("type"), "limit or market")),
        };
        // A market order names no limit, but a reference price for the caps.
        let price_field = if market { "ref_price" } else { "limit_price" };
        let price_path = f.at(price_field);
        let limit_cents = parse_cents(&f.string(price_field)?)
            .filter(|c| if combo { *c != 0 } else { *c > 0 })
            .ok_or_else(|| {
                invalid(
                    &price_path,
                    "decimal, at most 2 places; single > 0, combo != 0",
                )
            })?;
        let on_close = match f.string("time_in_force")?.as_str() {
            "day" => false,
            "cls" if !combo && legs[0].equity && !market => true,
            _ => {
                return Err(invalid(
                    &f.at("time_in_force"),
                    "day, or cls for a stock single limit",
                ));
            }
        };
        f.finish()?;
        Ok(Self {
            client_order_id,
            combo,
            legs,
            qty,
            limit_cents,
            market,
            on_close,
        })
    }

    /// Whether this is a stock order (a single equity leg).
    #[must_use]
    pub fn is_equity(&self) -> bool {
        !self.combo && self.legs[0].equity
    }

    /// Dollars per unit of price: 100 for an option contract, 1 for a share.
    #[must_use]
    pub fn multiplier(&self) -> u64 {
        if self.is_equity() { 1 } else { 100 }
    }

    /// Whether any leg opens a position.
    #[must_use]
    pub fn has_open(&self) -> bool {
        self.legs.iter().any(|l| l.open)
    }

    /// The canonical intent: fixed field order, no whitespace, two-place prices.
    #[must_use]
    pub fn canonical(&self) -> String {
        let instrument = |l: &Leg| {
            Obj::new()
                .s("class", if l.equity { "equity" } else { "option" })
                .s("symbol", &l.symbol)
                .done()
        };
        let side = |l: &Leg| if l.buy { "buy" } else { "sell" };
        let effect = |l: &Leg| if l.open { "open" } else { "close" };
        let mut o = Obj::new().s("client_order_id", &self.client_order_id);
        if self.combo {
            let legs = arr(self.legs.iter().map(|l| {
                Obj::new()
                    .raw("instrument", &instrument(l))
                    .n("ratio", l.ratio)
                    .s("side", side(l))
                    .s("effect", effect(l))
                    .done()
            }));
            o = o.s("shape", "combo").raw("legs", &legs);
        } else {
            let l = &self.legs[0];
            o = o
                .s("shape", "single")
                .raw("instrument", &instrument(l))
                .s("side", side(l))
                .s("effect", effect(l));
        }
        o.n("qty", self.qty)
            .s("type", if self.market { "market" } else { "limit" })
            .s(
                if self.market {
                    "ref_price"
                } else {
                    "limit_price"
                },
                &fmt_cents(self.limit_cents),
            )
            .s("time_in_force", if self.on_close { "cls" } else { "day" })
            .done()
    }

    /// Lowercase hex SHA-256 of [`Intent::canonical`].
    #[must_use]
    pub fn sha256(&self) -> String {
        hex(&Sha256::digest(self.canonical().as_bytes()))
    }
}

/// What an opening intent can lose, and what it costs to open.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaxLoss {
    /// The most the intent can lose, in cents, for its whole quantity.
    pub cents: u64,
    /// The debit the buyer pays, in cents, for its whole quantity. `None` for a credit,
    /// a short leg, or any intent with no debit.
    pub premium_cents: Option<u64>,
}

/// The most an opening intent can lose, or the reason it cannot be bounded. A long option or
/// share loses its premium; a naked short call or share is `max_loss_unbounded`; a naked
/// short put loses its strike less the credit. A combo of options with one underlying and
/// one expiry loses its net debit less the least payoff at expiry (see [`combo_loss`]).
/// A combo with a closing leg, and a calendar, are `max_loss_unknown`. A partial close is
/// not an intent property; the caller passes the open quantity that remains, since the loss
/// is linear in quantity and zero once nothing remains open.
pub fn max_loss(intent: &Intent) -> Result<MaxLoss, &'static str> {
    let limit = i128::from(intent.limit_cents);
    let (per_unit, debit) = if intent.combo {
        (combo_loss(intent)?, intent.limit_cents > 0)
    } else {
        (single_loss(&intent.legs[0], limit)?, intent.legs[0].buy)
    };
    let mult = i128::from(intent.multiplier());
    let qty = i128::from(intent.qty);
    // Nothing left open (a full close of the opening quantity) has no loss.
    let cents = if intent.qty == 0 {
        0
    } else {
        u64::try_from(per_unit * qty * mult)
            .ok()
            .filter(|v| *v > 0)
            .ok_or("max_loss_unknown")?
    };
    let premium_cents = debit
        .then(|| u64::try_from(limit * qty * mult).ok())
        .flatten();
    Ok(MaxLoss {
        cents,
        premium_cents,
    })
}

/// The loss per unit of a single opening leg, in cents of price (signed).
fn single_loss(leg: &Leg, limit: i128) -> Result<i128, &'static str> {
    if !leg.open {
        return Err("max_loss_unknown");
    }
    if leg.buy {
        return Ok(limit);
    }
    if leg.equity {
        return Err("max_loss_unbounded");
    }
    match contract(&leg.symbol) {
        Some(c) if c.put => Ok(strike_of(&c)? - limit),
        Some(_) => Err("max_loss_unbounded"),
        None => Err("max_loss_unknown"),
    }
}

/// Index roots whose options are European: they exercise only at expiry, so their value before
/// expiry is discounted and can sit below the intrinsic payoff. SPXW is the weekly SPX root,
/// and VIXW the weekly VIX root. The list must match the contract specs: an unlisted root is
/// treated as American. The payoff-floor bound of [`combo_loss`] and the put-calendar bound of
/// [`calendar_loss`] depend on this.
const EUROPEAN_ROOTS: [&str; 9] = [
    "SPX", "SPXW", "XSP", "NDX", "XND", "RUT", "MRUT", "VIX", "VIXW",
];

/// The loss per parent unit of a combo, in cents of price (signed). The payoff at expiry is
/// piecewise linear with breakpoints at the strikes and at zero, so its least value is at
/// one of those points, provided it does not keep falling above the highest strike. The
/// limit is signed (+ debit, - credit), so the loss is the limit less that least payoff:
/// a $1.00 credit against a least payoff of -$5.00 loses $4.00.
///
/// Each leg pays max(S - K, 0) for a call and max(K - S, 0) for a put, per unit of its
/// ratio and signed by its side (+ bought, - sold). The slope above the highest strike is
/// the sum of the signed call ratios; if it is negative the loss is unbounded.
///
/// A combo with a closing leg is unknown. The intent carries one net limit for every leg
/// and no price per leg, so the closing legs' share of it cannot be read. Nothing is
/// assumed about that split, so the opening legs are not sized alone. Legs with different
/// expiries are a calendar or diagonal (see [`calendar_loss`]).
///
/// The bound at the least payoff is sound only if the position is worth at least that payoff
/// before expiry. An American option always is (it can be exercised). A European one is not:
/// its value is discounted, so a long position with a payoff floor above zero can be worth less
/// than the floor before expiry. For a European root such a floor is not used, and the combo is
/// `max_loss_unknown`. A floor at zero is the debit, which holds for any option, and a negative
/// least payoff (a short-side bound) is no less than its discounted value, so both stay.
fn combo_loss(intent: &Intent) -> Result<i128, &'static str> {
    if intent.legs.iter().any(|l| !l.open) {
        return Err("max_loss_unknown");
    }
    let cs = intent
        .legs
        .iter()
        .map(|l| contract(&l.symbol).ok_or("max_loss_unknown"))
        .collect::<Result<Vec<_>, _>>()?;
    if cs.iter().any(|c| c.root != cs[0].root) {
        return Err("max_loss_unknown");
    }
    if cs.iter().any(|c| c.expiry != cs[0].expiry) {
        return calendar_loss(intent, &cs);
    }
    // Per leg: its strike in cents, whether it is a put, and its signed ratio.
    let parts = intent
        .legs
        .iter()
        .zip(&cs)
        .map(|(l, c)| {
            let w = i128::from(l.ratio);
            Ok((strike_of(c)?, c.put, if l.buy { w } else { -w }))
        })
        .collect::<Result<Vec<_>, &'static str>>()?;
    let slope_above: i128 = parts.iter().filter(|p| !p.1).map(|p| p.2).sum();
    if slope_above < 0 {
        return Err("max_loss_unbounded");
    }
    let payoff = |s: i128| -> i128 {
        parts
            .iter()
            .map(|&(k, put, w)| w * if put { (k - s).max(0) } else { (s - k).max(0) })
            .sum()
    };
    // Zero bounds the underlying below; every strike is a breakpoint.
    let least = parts
        .iter()
        .map(|p| p.0)
        .chain([0])
        .map(payoff)
        .min()
        .unwrap_or(0);
    if least > 0 && EUROPEAN_ROOTS.contains(&cs[0].root) {
        return Err("max_loss_unknown");
    }
    Ok(i128::from(intent.limit_cents) - least)
}

/// The loss per unit of a calendar or diagonal: one short and one long leg, one-to-one, of
/// one type, with the short expiring first. A long at least as favourable as the short (a
/// call at or below its strike, a put at or above) covers it, and the loss is the debit.
/// A less favourable long (a call above the short strike, a put below it) leaves the gap
/// between the strikes open, so the loss is the debit plus that gap. A short expiring after
/// its long is unknown, as is anything with more legs or ratios other than one.
///
/// A European put calendar is unknown whatever its strikes. Before expiry the long put is worth
/// its strike discounted, not its intrinsic value, so the position can lose more than the debit
/// plus the gap even when the long strike is at or above the short. European calls are bounded
/// this way: a call is worth at least its discounted forward intrinsic value.
fn calendar_loss(intent: &Intent, cs: &[Contract]) -> Result<i128, &'static str> {
    let (a, b) = match (intent.legs.as_slice(), cs) {
        ([a, b], [_, _]) => (a, b),
        _ => return Err("max_loss_unknown"),
    };
    if a.ratio != 1 || b.ratio != 1 || a.buy == b.buy {
        return Err("max_loss_unknown");
    }
    let (si, li) = if a.buy { (1, 0) } else { (0, 1) };
    let (short, long) = (&cs[si], &cs[li]);
    if short.put != long.put || short.expiry > long.expiry {
        return Err("max_loss_unknown");
    }
    if short.put && EUROPEAN_ROOTS.contains(&short.root) {
        return Err("max_loss_unknown");
    }
    let (ks, kl) = (strike_of(short)?, strike_of(long)?);
    // Positive when the long's strike is less favourable than the short's.
    let gap = if short.put { ks - kl } else { kl - ks };
    Ok(i128::from(intent.limit_cents) + gap.max(0))
}

fn strike_of(c: &Contract) -> Result<i128, &'static str> {
    c.strike_cents.map(i128::from).ok_or("max_loss_unknown")
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::{Intent, Leg, MaxLoss, max_loss};

    fn max_loss_cents(o: &Intent) -> Result<u64, &'static str> {
        max_loss(o).map(|m| m.cents)
    }

    const C575: &str = "SPY261005C00575000";
    const C580: &str = "SPY261005C00580000";
    const C585: &str = "SPY261005C00585000";
    const C580_NOV: &str = "SPY261012C00580000";
    const P565: &str = "SPY261005P00565000";
    const P570: &str = "SPY261005P00570000";
    const P570_ODD: &str = "SPY261005P00570005";
    const P580: &str = "SPY261005P00580000";
    const C570: &str = "SPY261005C00570000";
    const C585_NOV: &str = "SPY261012C00585000";

    fn leg(symbol: &str, buy: bool, open: bool) -> Leg {
        Leg {
            symbol: symbol.into(),
            equity: false,
            ratio: 1,
            buy,
            open,
        }
    }

    fn ratio_leg(symbol: &str, buy: bool, ratio: u32) -> Leg {
        Leg {
            ratio,
            ..leg(symbol, buy, true)
        }
    }

    fn share(buy: bool, open: bool) -> Leg {
        Leg {
            symbol: "AAPL".into(),
            equity: true,
            ratio: 1,
            buy,
            open,
        }
    }

    /// A limit intent: `limit` in cents, signed for a combo (+ debit, - credit).
    fn order(legs: Vec<Leg>, limit: i64, qty: u32) -> Intent {
        Intent {
            client_order_id: "wse-t".into(),
            combo: legs.len() > 1,
            legs,
            qty,
            limit_cents: limit,
            market: false,
            on_close: false,
        }
    }

    #[test]
    fn a_single_long_is_its_premium() {
        // $1.25 a contract, 10 contracts: $1,250, as before.
        let o = order(vec![leg(C580, true, true)], 125, 10);
        assert_eq!(max_loss_cents(&o), Ok(125 * 10 * 100));
        let s = order(vec![share(true, true)], 22_250, 12);
        assert_eq!(max_loss_cents(&s), Ok(22_250 * 12));
        assert_eq!(max_loss(&o).unwrap().premium_cents, Some(125 * 10 * 100));
    }

    #[test]
    fn a_credit_spread_loses_its_width_less_the_credit() {
        // Sell 570 put, buy 565 put, $1.00 credit: $5 wide, so $4 a share at risk.
        let o = order(
            vec![leg(P570, false, true), leg(P565, true, true)],
            -100,
            10,
        );
        assert_eq!(max_loss_cents(&o), Ok(400 * 10 * 100));
        // A call credit spread: sell 580, buy 585 for $1.00.
        let c = order(vec![leg(C580, false, true), leg(C585, true, true)], -100, 1);
        assert_eq!(max_loss_cents(&c), Ok(400 * 100));
        // A credit is received, not paid: no premium.
        assert_eq!(max_loss(&o).unwrap().premium_cents, None);
    }

    #[test]
    fn a_debit_vertical_loses_its_debit() {
        // A call debit spread: buy 580, sell 585 for $1.50.
        let call = order(vec![leg(C580, true, true), leg(C585, false, true)], 150, 3);
        assert_eq!(max_loss_cents(&call), Ok(150 * 3 * 100));
        assert_eq!(max_loss(&call).unwrap().premium_cents, Some(150 * 3 * 100));
        // A put debit spread: buy 570, sell 565 for $2.00.
        let put = order(vec![leg(P570, true, true), leg(P565, false, true)], 200, 1);
        assert_eq!(max_loss_cents(&put), Ok(200 * 100));
    }

    #[test]
    fn a_debit_with_the_long_inside_loses_the_width_too() {
        // Buy 585, sell 580 calls for a $1.00 debit: the short leg is in the money first.
        let o = order(vec![leg(C585, true, true), leg(C580, false, true)], 100, 1);
        assert_eq!(max_loss_cents(&o), Ok((500 + 100) * 100));
    }

    #[test]
    fn a_credit_worth_more_than_the_width_is_unknown() {
        let o = order(vec![leg(P570, false, true), leg(P565, true, true)], -600, 1);
        assert_eq!(max_loss_cents(&o), Err("max_loss_unknown"));
    }

    #[test]
    fn an_iron_condor_loses_its_wider_wing_less_the_credit() {
        // Puts 565/570 (5 wide), calls 575/585 (10 wide), $2.00 credit.
        let o = order(
            vec![
                leg(P565, true, true),
                leg(P570, false, true),
                leg(C575, false, true),
                leg(C585, true, true),
            ],
            -200,
            1,
        );
        assert_eq!(max_loss_cents(&o), Ok((1000 - 200) * 100));
        // The same wings for a $2.00 debit: the payoff falls to -$10 above 585 (short 575,
        // long 585 calls), so the loss is the wider wing plus the debit: $12.
        let debit = order(
            vec![
                leg(P565, true, true),
                leg(P570, false, true),
                leg(C575, false, true),
                leg(C585, true, true),
            ],
            200,
            1,
        );
        assert_eq!(max_loss_cents(&debit), Ok((1000 + 200) * 100));
    }

    #[test]
    fn a_naked_short_call_or_share_is_unbounded() {
        let call = order(vec![leg(C580, false, true)], 125, 1);
        assert_eq!(max_loss_cents(&call), Err("max_loss_unbounded"));
        let short = order(vec![share(false, true)], 22_250, 1);
        assert_eq!(max_loss_cents(&short), Err("max_loss_unbounded"));
    }

    #[test]
    fn a_naked_short_put_is_its_strike_less_the_credit() {
        // Sell the 570 put for $1.00: $569 a share at risk.
        let o = order(vec![leg(P570, false, true)], 100, 2);
        assert_eq!(max_loss_cents(&o), Ok((57_000 - 100) * 2 * 100));
        assert_eq!(max_loss(&o).unwrap().premium_cents, None);
    }

    #[test]
    fn a_butterfly_with_a_debit_loses_the_debit() {
        // Buy 575, sell two 580, buy 585 calls for $1.00. The payoff is 0 at and below 575
        // and at and above 585, and peaks at $5 at 580, so its least payoff is 0.
        // Loss: $1.00 a share, $100 a contract.
        let fly = order(
            vec![
                leg(C575, true, true),
                ratio_leg(C580, false, 2),
                leg(C585, true, true),
            ],
            100,
            1,
        );
        assert_eq!(max_loss_cents(&fly), Ok(100 * 100));
        assert_eq!(max_loss(&fly).unwrap().premium_cents, Some(100 * 100));
    }

    #[test]
    fn a_butterfly_with_a_credit_loses_its_width_less_the_credit() {
        // Sell 575 and 585 calls, buy two 580 calls, for a $1.00 credit. At 580 the payoff
        // is -$5 (the short 575 is $5 in the money), and it is 0 at 575 and at 585. So the
        // least payoff is -$5: loss $5 - $1 = $4 a share, $400 a contract.
        let fly = order(
            vec![
                leg(C575, false, true),
                ratio_leg(C580, true, 2),
                leg(C585, false, true),
            ],
            -100,
            1,
        );
        assert_eq!(max_loss_cents(&fly), Ok(400 * 100));
        assert_eq!(max_loss(&fly).unwrap().premium_cents, None);
    }

    #[test]
    fn a_debit_condor_loses_its_debit() {
        // Buy the 570 put and the 575 call, sell the 565 put and the 585 call, for $2.00.
        // The payoff is between 0 and $10 and touches 0 on [570, 575], so the loss is the
        // $2.00 debit: $200 a contract.
        let condor = order(
            vec![
                leg(P570, true, true),
                leg(P565, false, true),
                leg(C575, true, true),
                leg(C585, false, true),
            ],
            200,
            1,
        );
        assert_eq!(max_loss_cents(&condor), Ok(200 * 100));
        assert_eq!(max_loss(&condor).unwrap().premium_cents, Some(200 * 100));
    }

    #[test]
    fn a_put_ratio_spread_is_bounded_and_a_call_ratio_spread_is_not() {
        // Buy one 580 put, sell two 570 puts, for a $1.00 credit. The payoff is 580 - 2 x 570
        // = -$560 at zero, and it rises to 0 at 580, so the least payoff is -$560 a share.
        // Loss: $560 - $1 = $559 a share, $55,900 a contract.
        let put = order(
            vec![leg(P580, true, true), ratio_leg(P570, false, 2)],
            -100,
            1,
        );
        assert_eq!(max_loss_cents(&put), Ok(559 * 100 * 100));
        // Buy one 575 call, sell two 585 calls, for a $1.00 debit: above 585 the payoff
        // falls by $1 for each dollar of the underlying, so the loss is unbounded.
        let call = order(
            vec![leg(C575, true, true), ratio_leg(C585, false, 2)],
            100,
            1,
        );
        assert_eq!(max_loss_cents(&call), Err("max_loss_unbounded"));
    }

    #[test]
    fn a_long_combo_with_a_payoff_floor_loses_less_than_its_debit() {
        // Buy the 580 put and the 570 call for $11.00. Between 570 and 580 the payoff is a
        // constant $10, so the least payoff is $10 and the loss is $1: $100 a contract.
        let o = order(vec![leg(P580, true, true), leg(C570, true, true)], 1100, 1);
        assert_eq!(max_loss_cents(&o), Ok(100 * 100));
    }

    #[test]
    fn a_long_combo_with_a_floor_is_unknown_on_a_european_root() {
        // The same shape as the SPY test above, on the European SPX, SPXW and XSP roots: the
        // floor is not used, so no bound is claimed. SPY keeps its bound of $1.
        for root in ["SPX", "SPXW", "XSP"] {
            let put = format!("{root}261005P00580000");
            let call = format!("{root}261005C00570000");
            let o = order(vec![leg(&put, true, true), leg(&call, true, true)], 1100, 1);
            assert_eq!(max_loss_cents(&o), Err("max_loss_unknown"), "{root}");
        }
        let spy = order(vec![leg(P580, true, true), leg(C570, true, true)], 1100, 1);
        assert_eq!(max_loss_cents(&spy), Ok(100 * 100));
    }

    #[test]
    fn a_covered_calendar_loses_its_debit() {
        // Sell the near 580 call and buy the November 580 call, for a $1.50 debit, two
        // contracts. The long covers the short at the same strike, so the loss is the debit:
        // $1.50 x 100 x 2 = $300.
        let cal = order(
            vec![leg(C580, false, true), leg(C580_NOV, true, true)],
            150,
            2,
        );
        assert_eq!(max_loss_cents(&cal), Ok(150 * 2 * 100));
        assert_eq!(max_loss(&cal).unwrap().premium_cents, Some(150 * 2 * 100));
    }

    #[test]
    fn a_diagonal_loses_its_debit_plus_the_strike_gap() {
        // Sell the near 580 call and buy the November 585 call, for a $1.50 debit. The long
        // is the less favourable strike, so the 585 - 580 = $5 gap is open: the loss is
        // $1.50 + $5.00 = $6.50 a share, $650 a contract.
        let diag = order(
            vec![leg(C580, false, true), leg(C585_NOV, true, true)],
            150,
            1,
        );
        assert_eq!(max_loss_cents(&diag), Ok((150 + 500) * 100));
    }

    #[test]
    fn a_european_put_calendar_with_a_lower_long_strike_is_unknown() {
        // Sell the 580 put expiring 16 Oct and buy the 570 put expiring 23 Oct, for a $1.00
        // debit. On SPXW the long put is worth its discounted strike at a zero underlying, so
        // the loss exceeds the $11.00 debit-plus-gap bound: no bound is claimed.
        let cal = order(
            vec![
                leg("SPXW261016P00580000", false, true),
                leg("SPXW261023P00570000", true, true),
            ],
            100,
            1,
        );
        assert_eq!(max_loss_cents(&cal), Err("max_loss_unknown"));
        // The same on the other European roots, and with the long strike at or above the short.
        for root in ["SPX", "XSP", "NDX", "RUT", "VIX", "VIXW"] {
            let cal = order(
                vec![
                    leg(&format!("{root}261016P00580000"), false, true),
                    leg(&format!("{root}261023P00570000"), true, true),
                ],
                100,
                1,
            );
            assert_eq!(max_loss_cents(&cal), Err("max_loss_unknown"), "{root}");
        }
        let covered = order(
            vec![
                leg("SPXW261016P00570000", false, true),
                leg("SPXW261023P00580000", true, true),
            ],
            100,
            1,
        );
        assert_eq!(max_loss_cents(&covered), Err("max_loss_unknown"));
    }

    #[test]
    fn a_put_calendar_with_a_lower_long_strike_keeps_its_bound_on_spy() {
        // The same put calendar on SPY is American, so the bound stands: $1.00 debit plus the
        // $10 gap, $11.00 a contract.
        let cal = order(
            vec![
                leg("SPY261016P00580000", false, true),
                leg("SPY261023P00570000", true, true),
            ],
            100,
            1,
        );
        assert_eq!(max_loss_cents(&cal), Ok(1100 * 100));
    }

    #[test]
    fn a_european_call_calendar_keeps_its_bound() {
        // Sell the 580 call and buy the 585 call a week later, on SPXW, for a $1.00 debit: the
        // $5 gap is open, so the loss is $6.00 a contract.
        let cal = order(
            vec![
                leg("SPXW261016C00580000", false, true),
                leg("SPXW261023C00585000", true, true),
            ],
            100,
            1,
        );
        assert_eq!(max_loss_cents(&cal), Ok((100 + 500) * 100));
    }

    #[test]
    fn a_short_expiring_after_its_long_is_unknown() {
        // Buy the near 580 call and sell the November 580 call: the short is not covered
        // by a later long, so no bound is claimed.
        let back = order(
            vec![leg(C580, true, true), leg(C580_NOV, false, true)],
            150,
            1,
        );
        assert_eq!(max_loss_cents(&back), Err("max_loss_unknown"));
    }

    #[test]
    fn a_roll_is_unknown_because_its_limit_is_not_split() {
        // Close the 580 call and open the 585 call: one net limit prices both legs, and the
        // intent gives no price for each, so the opening leg's loss cannot be sized alone.
        let roll = order(vec![leg(C580, false, false), leg(C585, true, true)], 100, 1);
        assert_eq!(max_loss_cents(&roll), Err("max_loss_unknown"));
        let roll_credit = order(
            vec![leg(C580, false, false), leg(C585, true, true)],
            -100,
            1,
        );
        assert_eq!(max_loss_cents(&roll_credit), Err("max_loss_unknown"));
    }

    #[test]
    fn a_shape_that_is_not_classified_is_unknown() {
        // A calendar: the legs expire on different dates.
        let cal = order(
            vec![leg(C580, true, true), leg(C580_NOV, false, true)],
            100,
            1,
        );
        assert_eq!(max_loss_cents(&cal), Err("max_loss_unknown"));
        // A strike with a fraction of a cent, and a symbol that is not an option.
        let odd = order(vec![leg(P570_ODD, false, true)], 100, 1);
        assert_eq!(max_loss_cents(&odd), Err("max_loss_unknown"));
        let bad = order(vec![leg("SPY261005X00580000", false, true)], 100, 1);
        assert_eq!(max_loss_cents(&bad), Err("max_loss_unknown"));
    }

    #[test]
    fn a_long_combo_of_options_loses_its_debit() {
        // A long straddle: buy the 580 call and the 570 put for $10.00.
        let o = order(vec![leg(C580, true, true), leg(P570, true, true)], 1000, 1);
        assert_eq!(max_loss_cents(&o), Ok(1000 * 100));
    }

    #[test]
    fn a_partial_close_leaves_the_remaining_open_quantity() {
        // Long 10 calls at $1.25, 4 closed: the remaining 6 are what can still be lost.
        let full = order(vec![leg(C580, true, true)], 125, 10);
        let rest = order(vec![leg(C580, true, true)], 125, 6);
        assert_eq!(max_loss_cents(&full), Ok(125 * 10 * 100));
        assert_eq!(max_loss_cents(&rest), Ok(125 * 6 * 100));
        assert_eq!(max_loss(&rest).unwrap().premium_cents, Some(125 * 6 * 100));
    }

    #[test]
    fn a_full_close_leaves_no_loss() {
        // Nothing remains open, so the loss and the premium are zero: a long option paid
        // nothing more, and a credit spread has no loss left.
        let long = order(vec![leg(C580, true, true)], 125, 0);
        assert_eq!(
            max_loss(&long),
            Ok(MaxLoss {
                cents: 0,
                premium_cents: Some(0)
            })
        );
        let spread = order(vec![leg(P570, false, true), leg(P565, true, true)], -100, 0);
        assert_eq!(max_loss_cents(&spread), Ok(0));
    }

    #[test]
    fn a_closing_leg_is_never_netted_against_an_opening_leg() {
        // A close beside an open is one limit for both, so neither leg is sized alone.
        // The close does not offset the open put, and the open does not offset the close.
        let credit = order(
            vec![leg(P570, false, true), leg(P565, true, false)],
            -100,
            1,
        );
        assert_eq!(max_loss_cents(&credit), Err("max_loss_unknown"));
        let debit = order(vec![leg(C580, true, true), leg(C585, false, false)], 150, 1);
        assert_eq!(max_loss_cents(&debit), Err("max_loss_unknown"));
        // A closing leg alone is never an opening intent, so it has no loss to bound.
        let close = order(vec![leg(C580, false, false)], 125, 1);
        assert_eq!(max_loss_cents(&close), Err("max_loss_unknown"));
    }

    #[test]
    fn a_naked_call_beside_a_long_put_is_unknown_not_a_vertical() {
        // A call sold with a long call is a vertical, not a naked call.
        let covered = order(vec![leg(C580, false, true), leg(C585, true, true)], -100, 1);
        assert_eq!(max_loss_cents(&covered), Ok(400 * 100));
        // A naked short call next to a long put is unbounded above the call strike.
        let mixed = order(vec![leg(C580, false, true), leg(P570, true, true)], -100, 1);
        assert_eq!(max_loss_cents(&mixed), Err("max_loss_unbounded"));
    }
}
