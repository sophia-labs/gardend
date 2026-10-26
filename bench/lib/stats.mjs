// Seeded PRNG, summary statistics, bootstrap ratio CI.
export function mulberry32(seed) {
  let a = seed >>> 0
  return () => { a = (a + 0x6d2b79f5) >>> 0; let t = a; t = Math.imul(t ^ (t >>> 15), t | 1); t ^= t + Math.imul(t ^ (t >>> 7), t | 61); return ((t ^ (t >>> 14)) >>> 0) / 4294967296 }
}
export const quantile = (sortedAsc, q) => {
  if (!sortedAsc.length) return 0
  const pos = (sortedAsc.length - 1) * q, lo = Math.floor(pos), hi = Math.ceil(pos)
  return sortedAsc[lo] + (sortedAsc[hi] - sortedAsc[lo]) * (pos - lo)
}
export const median = (xs) => quantile([...xs].sort((a, b) => a - b), 0.5)
export const r2 = (x) => Math.round(x * 100) / 100
export function summarize(xs) {
  if (!xs.length) return { mean: 0, p50: 0, p95: 0, p99: 0, max: 0, min: 0, mad: 0 }
  const s = [...xs].sort((a, b) => a - b), p50 = quantile(s, 0.5)
  const mad = quantile(s.map((x) => Math.abs(x - p50)).sort((a, b) => a - b), 0.5)
  return { mean: r2(xs.reduce((a, b) => a + b, 0) / xs.length), p50: r2(p50), p95: r2(quantile(s, 0.95)), p99: r2(quantile(s, 0.99)), max: r2(s.at(-1)), min: r2(s[0]), mad: r2(mad) }
}
export function pctls(xs, ps = [0.5, 0.95]) { const s = [...xs].sort((a, b) => a - b); return ps.map((p) => quantile(s, p)) }
// Bootstrap 95% CI of stat(B)/stat(A), seeded. stat defaults to the median.
export function bootstrapRatio(a, b, { stat = median, resamples = 10000, seed = 20261025 } = {}) {
  const rnd = mulberry32(seed), ra = stat(a), rb = stat(b)
  const draw = (xs) => { const out = new Array(xs.length); for (let i = 0; i < xs.length; i++) out[i] = xs[Math.floor(rnd() * xs.length)]; return out }
  const ratios = []
  for (let i = 0; i < resamples; i++) { const da = stat(draw(a)); if (da === 0) continue; ratios.push(stat(draw(b)) / da) }
  ratios.sort((x, y) => x - y)
  return { ratio: ra === 0 ? NaN : rb / ra, lo: quantile(ratios, 0.025), hi: quantile(ratios, 0.975) }
}
export const p95stat = (xs) => quantile([...xs].sort((a, b) => a - b), 0.95)
export function verdict({ ratio, lo, hi }) {
  if (!Number.isFinite(ratio)) return 'n/a'
  if ((lo <= 1 && hi >= 1) || Math.abs(ratio - 1) < 0.05) return 'same'
  return ratio < 1 ? 'faster' : 'slower'
}
// Least-squares slope of y on x.
export function slope(ys) {
  const n = ys.length; if (n < 2) return 0
  const mx = (n - 1) / 2, my = ys.reduce((a, b) => a + b, 0) / n
  let num = 0, den = 0
  for (let i = 0; i < n; i++) { num += (i - mx) * (ys[i] - my); den += (i - mx) ** 2 }
  return num / den
}
