import { paragraphs } from '../../lib/fixture.mjs'
export const IT = (smoke, standard, conc = 1) => ({ iterations: standard[0], warmup: standard[1], concurrency: conc, smoke: { iterations: smoke[0], warmup: smoke[1] } })
export const small = (ctx, i) => { const d = ctx.fixture.docs; return d[((i % d.length) + d.length) % d.length] }
export const SMOKE = ['smoke', 'standard']
export { paragraphs }
