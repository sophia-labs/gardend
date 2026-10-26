// Minimal text-layer PDF writer: no dependencies, real text objects (BT/ET with Tj), N pages, deterministic from the seed.
// Words come from a made-up syllable lexicon, so no real text can leak in.
import { mulberry32 } from '../../lib/stats.mjs'
import { lexicon } from '../../lib/fixture.mjs'

const esc = (s) => s.replace(/[\\()]/g, '\\$&')

// Returns a Buffer. opts: { pages, seed, linesPerPage = 40, wordsPerLine = 9, title }
export function makeTextPdf({ pages = 5, seed = 1, linesPerPage = 40, wordsPerLine = 9, title = 'gbench synthetic' } = {}) {
  const rnd = mulberry32(seed ^ 0x51ed), lex = lexicon(seed, 300)
  const objs = [] // index = object number - 1, value = body string
  const add = (body) => { objs.push(body); return objs.length }
  const catalog = add('<< /Type /Catalog /Pages 2 0 R >>') // 1
  add('') // 2: Pages, filled below
  const font = add('<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>') // 3
  const kids = []
  for (let p = 0; p < pages; p++) {
    let s = 'BT\n/F1 11 Tf\n14 TL\n72 740 Td\n'
    s += `(${esc(`${title} page ${p + 1} of ${pages}`)}) Tj\nT*\n`
    for (let l = 0; l < linesPerPage; l++) {
      const words = []; for (let w = 0; w < wordsPerLine; w++) words.push(lex[Math.floor(rnd() * lex.length)])
      s += `(${esc(words.join(' '))}) Tj\nT*\n`
    }
    s += 'ET'
    const content = add(`<< /Length ${Buffer.byteLength(s, 'latin1')} >>\nstream\n${s}\nendstream`)
    const page = add(`<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 ${font} 0 R >> >> /Contents ${content} 0 R >>`)
    kids.push(`${page} 0 R`)
  }
  objs[1] = `<< /Type /Pages /Kids [${kids.join(' ')}] /Count ${pages} >>`
  let out = '%PDF-1.4\n%\xE2\xE3\xCF\xD3\n'
  const offs = []
  objs.forEach((b, i) => { offs.push(Buffer.byteLength(out, 'latin1')); out += `${i + 1} 0 obj\n${b}\nendobj\n` })
  const xref = Buffer.byteLength(out, 'latin1')
  out += `xref\n0 ${objs.length + 1}\n0000000000 65535 f \n` + offs.map((o) => String(o).padStart(10, '0') + ' 00000 n \n').join('')
  out += `trailer\n<< /Size ${objs.length + 1} /Root ${catalog} 0 R >>\nstartxref\n${xref}\n%%EOF\n`
  return Buffer.from(out, 'latin1')
}
