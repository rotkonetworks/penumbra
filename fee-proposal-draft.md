# proposal: the 5th dimension - penumbra fee economics

how many cryptographers does it take to design sustainable economics? apparently
more or less than penumbra labs had. this is proposal about introducing
artificial scarcity to the system with governance-led policy.

## abstract

penumbra currently operates as a charity. users pay $0.000009 for private ibc
while relayers subsidize their exits 2,200x. this proposal examines penumbra's
4-dimensional fee structure and proposes targeting ibc withdrawals specifically -
our primary service to the external world - while keeping internal usage cheap.

## the product we sell

**to internal users:** private transactions, swaps, staking, governance.

**to the external world:** privacy-as-a-service for ibc assets. bridge in,
hold privately, withdraw when ready. that's the core product. currently free.
relayer fees considered, our community pays for it.

## how penumbra fees work

### the four dimensions

```
gas {
    block_space:         u64,  // bytes in blocks
    compact_block_space: u64,  // light client data
    verification:        u64,  // zk-snark verification
    execution:           u64,  // state machine ops
}
```

current prices: `block_space: 60, compact_block: 1556, verification: 1, execution: 1`

fee = sum of (price × gas) / 1000 per dimension.

### per-action gas costs

| action | block | compact | verif | exec |
|--------|-------|---------|-------|------|
| spend | 352 | 34 | 1000 | 10 |
| output | 560 | 240 | 1000 | 10 |
| swap | ~800 | 272 | 1000 | 10 |
| delegate | ~150 | 0 | 0 | 10 |
| **ics20_withdrawal** | **~250** | **0** | **0** | **10** |

ibc withdrawals are the cheapest action - zero verification, zero compact block.

### where fees go

**fees are burned.** more usage = scarcer supply = sound money.

crypto adoption runs on number-go-up technology.

burns → scarcity → price → attention → adoption. a well-funded penumbra does
more for privacy than a technically-pure ghost chain.

think eberhard/tesla phasing: roadster -> model s -> model 3. start premium,
scale to mass adoption.

now we are selling the gucci of privacy, and our fees should match that luxury.
well, as long as our user base is limited. and in success case scaling to larger
user bases would be just reducing fees via governance. some perks of having the
most scalable design in all of the crypto.

## tokenomics

### supply distribution

| category | amount | % |
|----------|--------|---|
| shielded pool (private wallets) | ~53m um | 53% |
| community pool (genesis allocation) | ~25m um | 25% |
| staked | ~18.7m um | 19% |
| transparent/ibc bridged out | ~3.65m um | 3.6% |
| **total supply** | **~100.5m um** | |

### inflation reality

| metric | value |
|--------|-------|
| issuance rate | 63,411 µum/block (fixed) |
| monthly issuance | ~33k um |
| yearly issuance | ~394k um |
| inflation rate | **~0.39%/year** |
| staking ratio | ~18.7% |
| staker apy | ~2.1% |

**we're not overpaying for security.** at current prices (~$0.015), the entire
validator set of ~40 nodes running 250gb infrastructure earns about **$500/month
combined**. running a penumbra validator is already an altruistic act.

### why hard caps make no sense for penumbra

there is no benefit in capping supply with penumbra's economic design:

1. **fees get burned** - every transaction reduces supply
2. **inflation is already minimal** - 0.39%/year is negligible
3. **max rate is self-limiting** - staker rewards cap at ~2% apy at 100% staking
4. **no schelling point needed** - bitcoin needed 21m as a meme anchor for
   sound money marketing. penumbra can achieve actual deflation through usage.

a hard cap would require halvening economics that make no sense when your burn
mechanism already exists. why artificially reduce validator rewards when you
can achieve the same outcome (and better) by increasing fee burns?

each validator chooses their own "taxation rate" via community pool funding
streams - currently 23/37 validators contribute ~1%, sending about **200 um/month
(~$3)** to the community pool. the real funding needs to come from fees, not
from squeezing already underpaid infrastructure.

### the burn gap

| metric | value |
|--------|-------|
| total burned (all time) | ~75k um |
| monthly issuance | ~33k um |
| burn vs issuance | **printing 5x faster than burning** |

the burn mechanism is decorative at current fee levels. but this is fixable
through fee policy, not supply caps.

## current state

a withdrawal costs ~500 upenumbra = $0.000009.

| chain | withdrawal fee | service |
|-------|----------------|---------|
| noble | $0.01-0.02 | transparent ibc |
| osmosis | $0.002 | transparent ibc |
| **penumbra** | **$0.000009** | **private ibc** |

we charge 1000x less than noble for infinitely more privacy.

### the relayer subsidy

user pays $0.000009, relayer pays ~$0.02 on noble. **subsidy ratio: 2,200x.**

at 1,500 monthly withdrawals: **$0.15/year burned.** we literally pay people to
extract value.

## the asymmetry argument

| action | economic impact | optimal fee |
|--------|-----------------|-------------|
| deposit | value flows in | low/free |
| internal tx | neutral | moderate |
| withdrawal | value flows out | higher |

deposits bring liquidity. withdrawals extract it after using our privacy.
we charge the same near-zero fee for both. this utterly lacks any economic
design and is solely calculated to enable maximum usage of the chain. well the
usage is not there yet so we should charge accordingly.

## the problem

governance can only change prices (global multipliers). all actions have
`execution: 10`, so increasing `execution_price` affects everything equally.
we can't target withdrawals without code changes.

## proposed solution

### phase 1: governance signal

raise `execution_price: 1 -> 100` via governance.

**context:** fee parameters were last tuned when um traded at ~$1.50. now at ~$0.015,
that's a 100x price drop. a 100x increase in execution_price just restores original
usd-equivalent fees.

**effect on all actions:**

| action | current fee | after 100x exec_price | usd change |
|--------|-------------|----------------------|------------|
| send (spend+output) | ~500 upen | ~510 upen | +$0.00000015 |
| swap | ~530 upen | ~540 upen | +$0.00000015 |
| delegate | ~150 upen | ~160 upen | +$0.00000015 |
| withdrawal | ~500 upen | ~510 upen | +$0.00000015 |

all actions have `execution: 10`, so 100x price increase adds ~10 upen per action.
at current um price: **+$0.00000015 per tx.** still unnoticeable.

**purpose:** tests community willingness for sustainable economics before investing
dev time. if governance rejects even this minimal change, no point proceeding.

### phase 2: code upgrade

change `ics20_withdrawal.execution: 10 -> 5_000_000`:

```rust
// crates/core/transaction/src/gas.rs
impl gas_cost for ics20_withdrawal {
    fn gas_cost(&self) -> gas {
        gas {
            block_space: self.encode_to_vec().len() as u64,
            compact_block_space: 0,
            verification: 0,
            execution: 5_000_000,  // was 10
        }
    }
}
```

with execution_price at 100 from phase 1:

| action | before | after | change |
|--------|--------|-------|--------|
| send | ~510 upen | ~510 upen | - |
| swap | ~540 upen | ~540 upen | - |
| **withdrawal** | **~510 upen** | **~500,000 upen** | **+1000x** |

withdrawal fee: $0.000009 -> $0.0075. internal usage unchanged.

governance can tune execution_price further (lower = cheaper withdrawals, higher = more burn).

### burn projections

| scenario | annual burn | % of issuance |
|----------|-------------|---------------|
| current | ~900 um | 0.045% |
| phase 1 only | ~1,000 um | 0.05% |
| phase 1+2 @ 1.5k/mo | ~9,000 um | 0.45% |
| phase 1+2 @ 15k/mo | ~90,000 um | 4.5% |
| at scale (150k/mo) | ~900,000 um | 45% |

## phase 3: the 5th dimension

hacking `execution` conflates resource cost with policy. proper solution: add a
5th "policy" dimension, governance-tunable per-action:

```
policy_costs: Map<ActionType, u64> {  // in chain state, not code
    ics20_withdrawal: 5_000_000,
    position_close: 100_000,
    spend: 0, output: 0, swap: 0, ...
}
```

**no code changes ever needed for fee adjustments** - pure governance.
to show that we are actually alive and well, we should drive for more active
onchain governance. enabling more actions for the community to vote on should be
positive. there's always bitcoin for stagnant economics.

implementation: ~1 week. backwards compatible (proto3 defaults to 0). we do seem
to lack a bit expertise nowadays so more likely to be bottlenecked by reviews
and broader testing.

### why this matters

- **hyperdeflation protection:** if burns exceed issuance, governance can lower
  fees without code changes
- **governance purpose:** with labs gone, what's there to vote on? this gives
  delegators real economic decisions
- **complete toolkit:** handles both current charity problem and future success

## counterarguments

**"higher fees reduce usage"** - users choose penumbra for privacy, not because
it's free. $0.01 won't change that.

**"wait for higher um price"** - might not happen, and more likely to happen
with more sound economics. circular. burns support price.

**"hurts adoption"** - subsidized usage isn't adoption. freeloaders aren't users.

## the path to ultrasound money

the best marketing penumbra could have is being **actually deflationary**. not
through artificial caps, but through real usage burning more than issuance.

**target: $15/day in fees** = ~1,000 um/day burned = ~30k um/month

at 33k um/month issuance, we'd be nearly supply-neutral. exceed that and we're
deflationary - true ultrasound money that scales by *lowering* fees via governance
as adoption grows.

this isn't utopian. it requires:
- ~1,500 withdrawals/day at $0.01 each, or
- ~150 withdrawals/day at $0.10 each

penumbra should default withdrawal pricing to approximately **2x relay costs**
to at least break even on the infrastructure we subsidize. cheaper fees for
high-volume routes like osmosis, premium for others.

as much as you might dislike nomic's percentage-based btc withdrawal fees,
you don't hesitate to pay them when you need no-strings-attached btc. privacy
has value. we should price it accordingly.

## conclusion

penumbra is one of the only privacy solutions for ibc. we charge nothing for it.
instead we've been running a charity while validators work for free.

the solution isn't supply caps or cutting validator rewards further. it's getting
**users to pay for the service we provide**. we're the gucci of privacy - time
to price like it.

tldr:
1. increase withdrawal fees: $0.000009 -> $0.01-0.02 (still cheap, covers relay costs)
2. internal usage: essentially unchanged
3. burn revenue: 1000x improvement
4. path to deflation without artificial caps
5. governance tunability preserved
