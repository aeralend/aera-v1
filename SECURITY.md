# Security policy

Aera is a lending protocol holding other people's money. If you have found
something, we want to hear about it before anyone else does.

## Reporting

**Privately, through a [GitHub security advisory](https://github.com/aeralend/aera-v1/security/advisories/new).**
Never a public issue, never a pull request that fixes it, never a post.

Include what you need to make the case and no more: the account or instruction
involved, the conditions, and — if you have one — a failing test against the
harness in `programs/aera/tests/`. A reproduction in that harness is the single
most useful thing you can send, because it can be run and argued about in
minutes rather than days.

You can expect an acknowledgement within **72 hours** and an assessment within
**7 days**. If either slips, that is a failure on our side and you should chase
it in the same thread.

There is no bug bounty programme. We are not going to imply one with vague
wording, and we would rather tell you that up front than after you have spent a
weekend on it.

## Scope

**In scope** — anything in this repository that can move funds, misprice
collateral, bypass a gate or brick a position:

- the instruction handlers in `programs/aera/src/instructions/`
- the oracle, its circuit breaker and its deployment pinning
- interest accrual, index arithmetic, and every rounding decision
- the health calculation and the liquidation split
- account validation, seeds, and anything an attacker can substitute
- the admin surface and its compiled ceilings

**Out of scope**, because it is not this repository or not ours:

- the interface, the SDK and the keepers — closed source. A finding in one of
  them is still welcome through the advisory form above; it is simply not a
  finding against the code in this repository
- the stake pool Aera reads its collateral rate from. Aera pins that program's
  deploy slot and upgrade authority and detects a replacement; it cannot
  prevent one, and a compromise there is a dependency risk rather than a
  finding against this code
- Cookie Chain itself, its validators and its RPC infrastructure
- anything requiring the protocol's admin key, which is a trust assumption
  documented rather than defended against

## Safe harbour

Research conducted in good faith under this policy is authorised, and we will
not pursue or support action against you for it. In good faith means:

- testing against a local validator, a testnet, or funds you own
- not accessing, modifying or exfiltrating anyone else's data or funds
- not degrading the service for anyone else
- reporting privately and giving us a reasonable window before disclosing

Deploying the program to a test network for research is explicitly permitted by
the licence, and this is exactly the case that permission exists for.

If you find yourself holding funds that are not yours as a result of testing,
tell us immediately and return them. Nothing in this policy authorises keeping
them.

## Disclosure

We would rather coordinate than negotiate. Our default is:

1. you report, we acknowledge
2. we agree severity and a fix window with you
3. we ship, and we tell affected users what happened
4. you publish, with credit, on a date we have agreed

**90 days** is our outside limit for anything not being actively exploited. If
we go past it without a reason you accept, publish — a deadline that only binds
the reporter is not a deadline.

If something is being exploited right now, say so in the first line of the
report and we will drop the process.
