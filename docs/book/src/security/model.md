# The security model

> **Draft outline.** This chapter is not written yet.

## Will cover

- Principals, actions, resources; deny by default.
- Policies become `PolicyFilter` barriers and `WriteCheck`s in the plan.
- Leakproof functions and what may move below a barrier.
- Transactional authorization: policy and membership reads are part of the transaction.

## Check against

- `crates/ir/src/security/`
- ADR-0015
- ADR-0018
- ADR-0019
