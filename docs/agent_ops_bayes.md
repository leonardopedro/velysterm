# Bayesian Ops — agent protocol extension

Two new ops (`bayesian_update`, `belief_propagation`) close the capability
gap so AI agents can drive the full QFM §8 posterior-inference loop.

## `bayesian_update`

Run Hamiltonian Monte Carlo (HMC) sampling to compute the posterior
distribution over the QFM tomographic state given a set of observations.

**Eligibility:** requires a QFM tomographic model
(`{ "kind": "qfm_tomography", ... }`). Non-QFM models return
`UK-5000` (internal error).

### Request

```json
{
  "id": "1",
  "op": "bayesian_update",
  "params": {
    "model_id": 1,
    "observations": [[1.0, 0.0], [0.0, 1.0]],
    "hmc_opts": {
      "leapfrog_steps": 10,
      "step_size": 0.01,
      "n_iterations": 1000,
      "burn_in": 100,
      "seed": 42
    }
  }
}
```

- `model_id` (u64) — model handle from `create_model`.
- `observations` (Vec<Vec<f64>>) — each inner vec is a single observation
  (flat array of measurement outcomes).
- `hmc_opts` (optional) — HMC hyperparameters. Defaults match
  `HmcOptsSpec::default()`.

### Response (success)

```json
{
  "id": "1",
  "ok": true,
  "result": {
    "log_posterior": -12.34,
    "mean_likelihood": 0.567,
    "image": [0.1, 0.2, ...],
    "posterior_mean_image": [0.11, 0.21, ...],
    "n_samples": 900,
    "n_observations": 2,
    "solve_ms": 1423
  }
}
```

| Field | Type | Description |
|---|---|---|
| `log_posterior` | f64 | HMC log-posterior at the final sample |
| `mean_likelihood` | f64 | Geometric mean of likelihoods; `-1.0` if no observations |
| `image` | Vec<f64> | Phase 5 reconstruction of the representative draw |
| `posterior_mean_image` | Vec<f64> | Karcher mean reconstruction; empty if no post-burn-in samples |
| `n_samples` | usize | Number of samples averaged into `posterior_mean_image` |
| `n_observations` | usize | Number of observations |
| `solve_ms` | u64 | Wall-clock time for HMC + decode |

### Failure codes

| Code | Condition |
|---|---|
| UK-1004 | Bad `model_id` handle |
| UK-1003 | Invalid `observations` JSON |
| UK-5000 | Non-QFM model |

### Events

On success, a `bayesian_updated` event is pushed to the model's event queue:

```json
{
  "type": "bayesian_updated",
  "log_posterior": -12.34,
  "mean_likelihood": 0.567,
  "n_observations": 2,
  "solve_ms": 1423
}
```

## `belief_propagation`

Run chain belief propagation (P8.8) for a fast posterior point estimate
without HMC sampling cost. Returns the MAP (marginal mode) point estimate
and the decoded full-resolution image.

**Eligibility:** requires a QFM tomographic model. Non-QFM models return
`UK-5000`.

### Request

```json
{
  "id": "2",
  "op": "belief_propagation",
  "params": {
    "model_id": 1,
    "observations": [[1.0, 0.0]],
    "opts": {
      "belief_propagation_rounds": 10
    }
  }
}
```

- `model_id` (u64) — model handle from `create_model`.
- `observations` (Vec<Vec<f64>>) — measurement observations.
- `opts` (optional) — BP hyperparameters. Defaults match
  `BeliefPropagationOptsSpec::default()`.

### Response (success)

```json
{
  "id": "2",
  "ok": true,
  "result": {
    "image": [0.1, 0.2, ...],
    "log_posterior": -5.67,
    "n_observations": 1,
    "n_sweeps": 1,
    "solve_ms": 234
  }
}
```

| Field | Type | Description |
|---|---|---|
| `image` | Vec<f64> | Phase 5 reconstructed image of the MAP |
| `log_posterior` | f64 | Log-posterior at the MAP (up to a constant) |
| `n_observations` | usize | Number of observations |
| `n_sweeps` | usize | Cumulative-product sweeps (always 1) |
| `solve_ms` | u64 | Wall-clock time for BP + decode |

### Failure codes

Same as `bayesian_update`: UK-1004, UK-1003, UK-5000.

### Events

On success, a `belief_propagated` event is pushed:

```json
{
  "type": "belief_propagated",
  "log_posterior": -5.67,
  "n_observations": 1,
  "solve_ms": 234
}
```

## UK code assignments

| Code | Name | Description |
|---|---|---|
| UK-5000 | InternalError | Generic internal error (e.g. QFM-required ops on non-QFM models) |

UK-5000 is defined in `unfer_protocol`. No new codes are needed for these
two ops.
