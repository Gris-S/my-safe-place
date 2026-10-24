import { useEffect, useState, type DragEvent } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { open } from '@tauri-apps/plugin-dialog'
import { ArrowRight, FolderOpen, GripVertical, Pencil, Plus, Trash2, TriangleAlert } from 'lucide-react'

type Rule = {
  id: string
  source: string
  target: string
  mount: string
  categories: string[]
  extensions: string[]
  minSize: number | null
  deleteFollows: boolean
  enabled: boolean
  createdAt: number
}
type Config = { rules: Rule[]; categories: Record<string, string[]> }
type Status = { unplugged: string[]; lastError: string | null }

const MB = 1024 * 1024
const emptyRule = (): Rule => ({
  id: '', source: '', target: '', mount: '', categories: [], extensions: [],
  minSize: null, deleteFollows: false, enabled: true, createdAt: 0,
})
const base = (p: string) => p.split('/').filter(Boolean).pop() ?? p

function useTheme() {
  useEffect(() => {
    const mq = matchMedia('(prefers-color-scheme: dark)')
    const apply = () => document.documentElement.setAttribute('data-theme', mq.matches ? 'dark' : 'light')
    apply()
    mq.addEventListener('change', apply)
    return () => mq.removeEventListener('change', apply)
  }, [])
}

export default function App() {
  useTheme()
  const [config, setConfig] = useState<Config | null>(null)
  const [status, setStatus] = useState<Status>({ unplugged: [], lastError: null })
  const [editing, setEditing] = useState<{ rule: Rule; isNew: boolean } | null>(null)
  const [preview, setPreview] = useState<{ ruleId: string; files: string[]; picked: Set<string> } | null>(null)
  const [error, setError] = useState('')
  const [dragFrom, setDragFrom] = useState<number | null>(null)

  useEffect(() => {
    invoke<{ config: Config; status: Status }>('get_state').then(s => { setConfig(s.config); setStatus(s.status) })
    const un = listen<Status>('status', e => setStatus(e.payload))
    return () => { un.then(f => f()) }
  }, [])

  if (!config) return null

  const save = async (next: Config) => {
    setError('')
    try {
      const saved = await invoke<Config>('save_config', { config: next })
      setConfig(saved)
      return saved
    } catch (e) {
      setError(String(e))
      return null
    }
  }

  const submit = async (rule: Rule, isNew: boolean, applyExisting: boolean) => {
    const rules = isNew ? [...config.rules, rule] : config.rules.map(r => (r.id === rule.id ? rule : r))
    const saved = await save({ ...config, rules })
    if (!saved) return
    setEditing(null)
    if (isNew && applyExisting) {
      const ruleId = saved.rules[saved.rules.length - 1].id
      const files = await invoke<string[]>('preview_existing', { ruleId })
      setPreview({ ruleId, files, picked: new Set(files) })
    }
  }

  const applyPreview = async () => {
    if (!preview) return
    try {
      await invoke('apply_existing', { ruleId: preview.ruleId, files: [...preview.picked] })
      setPreview(null)
    } catch (e) {
      setError(String(e))
    }
  }

  const drop = (to: number) => (e: DragEvent) => {
    e.preventDefault()
    if (dragFrom === null || dragFrom === to) return
    const rules = [...config.rules]
    const [moved] = rules.splice(dragFrom, 1)
    rules.splice(to, 0, moved)
    setDragFrom(null)
    save({ ...config, rules })
  }

  const unplugged = config.rules.filter(r => status.unplugged.includes(r.id))

  return (
    <div className={`app ${unplugged.length ? 'alarm' : ''}`}>
      <header>
        <h1>MY SAFE PLACE</h1>
        <p className="hint">Les fichiers qui correspondent à une règle partent dans la cible ; un lien reste à leur place. Pas de règle, on ne touche à rien.</p>
      </header>

      {unplugged.length > 0 && (
        <div className="banner" role="alert">
          <TriangleAlert size={28} />
          <div>
            <strong>Attention, tu as débranché un truc !</strong>
            {unplugged.map(r => <div key={r.id} className="mono">{r.target}</div>)}
            <div>Les nouveaux fichiers attendent dans le dossier de départ et partiront au rebranchement.</div>
          </div>
        </div>
      )}
      {(error || status.lastError) && <p className="error">{error || status.lastError}</p>}

      <section className="list">
        {config.rules.length === 0 && <p className="empty">Aucune règle : MY SAFE PLACE ne touche à rien.</p>}
        {config.rules.map((r, i) => (
          <div
            key={r.id}
            className={`card rule ${status.unplugged.includes(r.id) ? 'off' : ''} ${r.enabled ? '' : 'disabled'}`}
            draggable
            onDragStart={() => setDragFrom(i)}
            onDragOver={e => e.preventDefault()}
            onDrop={drop(i)}
          >
            <GripVertical className="grip" size={18} aria-label="Glisser pour changer l'ordre" />
            <div className="body">
              <div className="path">
                <span title={r.source}>{base(r.source)}</span>
                <ArrowRight size={16} />
                <span title={r.target}>{base(r.target)}</span>
              </div>
              <div className="chips">
                {r.categories.map(c => <span key={c} className="chip">{c}</span>)}
                {r.extensions.map(x => <span key={x} className="chip">.{x}</span>)}
                {r.minSize != null && <span className="chip">&gt; {fmtSize(r.minSize)}</span>}
                {r.deleteFollows && <span className="chip warn">suppr = suppr</span>}
              </div>
            </div>
            <label className="switch" title={r.enabled ? 'Règle active' : 'Règle en pause'}>
              <input type="checkbox" checked={r.enabled}
                onChange={e => save({ ...config, rules: config.rules.map(x => (x.id === r.id ? { ...x, enabled: e.target.checked } : x)) })} />
              <span />
            </label>
            <button aria-label="Modifier" onClick={() => setEditing({ rule: r, isNew: false })}><Pencil size={18} /></button>
            <button aria-label="Supprimer la règle" className="danger"
              onClick={() => confirm('Supprimer cette règle ? Les fichiers déjà rangés restent où ils sont.') && save({ ...config, rules: config.rules.filter(x => x.id !== r.id) })}>
              <Trash2 size={18} />
            </button>
          </div>
        ))}
        <button className="primary" onClick={() => setEditing({ rule: emptyRule(), isNew: true })}><Plus size={18} /> Nouvelle règle</button>
      </section>

      <Categories config={config} onSave={save} />

      {editing && <RuleEditor {...editing} categories={config.categories} onCancel={() => setEditing(null)} onSubmit={submit} />}

      {preview && (
        <div className="sheet-bg">
          <div className="sheet card">
            <h2>Fichiers déjà présents</h2>
            {preview.files.length === 0 ? <p className="hint">Aucun fichier existant ne correspond.</p> : (
              <ul className="files">
                {preview.files.map(f => (
                  <li key={f}>
                    <label>
                      <input type="checkbox" checked={preview.picked.has(f)} onChange={e => {
                        const picked = new Set(preview.picked)
                        if (e.target.checked) picked.add(f); else picked.delete(f)
                        setPreview({ ...preview, picked })
                      }} /> {base(f)}
                    </label>
                  </li>
                ))}
              </ul>
            )}
            <div className="actions">
              <button onClick={() => setPreview(null)}>Ne rien déplacer</button>
              {preview.picked.size > 0 && <button className="primary" onClick={applyPreview}>Déplacer {preview.picked.size} fichier{preview.picked.size > 1 ? 's' : ''}</button>}
            </div>
          </div>
        </div>
      )}
    </div>
  )
}

function fmtSize(b: number) {
  return b >= 1024 * MB ? `${+(b / (1024 * MB)).toFixed(2)} Go` : `${+(b / MB).toFixed(1)} Mo`
}

async function pickFolder(): Promise<string | null> {
  const p = await open({ directory: true })
  return typeof p === 'string' ? p : null
}

function RuleEditor({ rule: initial, isNew, categories, onCancel, onSubmit }: {
  rule: Rule; isNew: boolean; categories: Record<string, string[]>
  onCancel: () => void; onSubmit: (r: Rule, isNew: boolean, applyExisting: boolean) => void
}) {
  const [rule, setRule] = useState(initial)
  const [exts, setExts] = useState(initial.extensions.join(', '))
  const [size, setSize] = useState(initial.minSize == null ? '' : String(initial.minSize >= 1024 * MB ? initial.minSize / (1024 * MB) : initial.minSize / MB))
  const [unit, setUnit] = useState(initial.minSize != null && initial.minSize < 1024 * MB ? MB : 1024 * MB)
  const [applyExisting, setApplyExisting] = useState(false)

  const toggleCat = (c: string) =>
    setRule({ ...rule, categories: rule.categories.includes(c) ? rule.categories.filter(x => x !== c) : [...rule.categories, c] })

  const finish = () => {
    const extensions = exts.split(/[\s,;]+/).map(x => x.replace(/^\./, '').toLowerCase()).filter(Boolean)
    const n = parseFloat(size.replace(',', '.'))
    onSubmit({ ...rule, extensions, minSize: Number.isFinite(n) && n > 0 ? Math.round(n * unit) : null }, isNew, applyExisting)
  }

  const hasFilter = rule.categories.length > 0 || exts.trim() !== '' || size.trim() !== ''

  return (
    <div className="sheet-bg">
      <div className="sheet card">
        <h2>{isNew ? 'Nouvelle règle' : 'Modifier la règle'}</h2>

        <label className="field">Dossier de départ
          <button className="picker" onClick={async () => { const p = await pickFolder(); if (p) setRule({ ...rule, source: p }) }}>
            <FolderOpen size={18} /> <span className="mono">{rule.source || 'Choisir…'}</span>
          </button>
        </label>
        <label className="field">Dossier cible
          {/* Une cible déjà enregistrée garde son disque : on ne la change pas pendant qu'il est débranché. */}
          <button className="picker" disabled={!isNew} onClick={async () => { const p = await pickFolder(); if (p) setRule({ ...rule, target: p }) }}>
            <FolderOpen size={18} /> <span className="mono">{rule.target || 'Choisir…'}</span>
          </button>
          {!isNew && <span className="hint">Pour changer de cible, crée une nouvelle règle.</span>}
        </label>

        <div className="field">Catégories
          <div className="chips">
            {Object.keys(categories).map(c => (
              <button key={c} className={`chip toggle ${rule.categories.includes(c) ? 'on' : ''}`} onClick={() => toggleCat(c)} aria-pressed={rule.categories.includes(c)}>{c}</button>
            ))}
          </div>
        </div>
        <label className="field">Extensions en plus
          <input value={exts} onChange={e => setExts(e.target.value)} placeholder="stl, blend…" />
        </label>
        <label className="field">Plus gros que
          <span className="row">
            <input inputMode="decimal" value={size} onChange={e => setSize(e.target.value)} placeholder="(pas de limite)" />
            <select value={unit} onChange={e => setUnit(+e.target.value)}>
              <option value={MB}>Mo</option>
              <option value={1024 * MB}>Go</option>
            </select>
          </span>
        </label>

        <label className="check">
          <input type="checkbox" checked={rule.deleteFollows} onChange={e => setRule({ ...rule, deleteFollows: e.target.checked })} />
          <span><strong>suppr = suppr</strong> — supprimer le lien dans le départ envoie le vrai fichier à la corbeille du disque cible.</span>
        </label>
        {isNew && (
          <label className="check">
            <input type="checkbox" checked={applyExisting} onChange={e => setApplyExisting(e.target.checked)} />
            <span>Appliquer aussi aux fichiers déjà présents (aperçu avant de déplacer)</span>
          </label>
        )}

        <div className="actions">
          <button onClick={onCancel}>Annuler</button>
          <button className="primary" disabled={!rule.source || !rule.target || !hasFilter} onClick={finish}>Enregistrer</button>
        </div>
      </div>
    </div>
  )
}

function Categories({ config, onSave }: { config: Config; onSave: (c: Config) => void }) {
  const [draft, setDraft] = useState<Record<string, string>>({})
  return (
    <details className="card cats">
      <summary>Catégories et extensions</summary>
      {Object.entries(config.categories).map(([name, list]) => (
        <label key={name} className="field">{name}
          <input
            value={draft[name] ?? list.join(', ')}
            onChange={e => setDraft({ ...draft, [name]: e.target.value })}
            onBlur={() => {
              if (draft[name] === undefined) return
              const exts = draft[name].split(/[\s,;]+/).map(x => x.replace(/^\./, '').toLowerCase()).filter(Boolean)
              onSave({ ...config, categories: { ...config.categories, [name]: exts } })
              setDraft(d => { const { [name]: _, ...rest } = d; return rest })
            }}
          />
        </label>
      ))}
    </details>
  )
}
