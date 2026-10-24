// Le moteur : une règle = dossier de départ + dossier cible + filtres. Un fichier
// nouveau qui correspond part dans la cible (copie → vérification → suppression)
// et un lien symbolique absolu le remplace dans le départ. Pas de règle : rien.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Rule {
    pub id: String,
    pub source: PathBuf,
    pub target: PathBuf,
    /// Point de montage de la cible, relevé à la création : s'il n'est plus
    /// monté, la cible est « débranchée » même si un dossier du même nom existe.
    pub mount: PathBuf,
    pub categories: Vec<String>,
    pub extensions: Vec<String>,
    pub min_size: Option<u64>,
    pub delete_follows: bool,
    pub enabled: bool,
    /// Secondes Unix. Seuls les fichiers arrivés après (ctime) sont concernés.
    pub created_at: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Config {
    pub rules: Vec<Rule>,
    pub categories: BTreeMap<String, Vec<String>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Link {
    pub link: PathBuf,
    pub target: PathBuf,
    pub rule: String,
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// Règles dont la cible est débranchée.
    pub unplugged: Vec<String>,
    pub last_error: Option<String>,
}

pub fn default_categories() -> BTreeMap<String, Vec<String>> {
    let c = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    BTreeMap::from([
        ("Images".into(), c(&["jpg", "jpeg", "png", "gif", "webp", "heic", "avif", "bmp", "tiff", "svg", "raw", "cr2", "nef", "dng"])),
        ("PDF".into(), c(&["pdf"])),
        ("Vidéos".into(), c(&["mp4", "mkv", "mov", "avi", "webm", "m4v", "wmv"])),
        ("Musique".into(), c(&["mp3", "flac", "wav", "ogg", "opus", "m4a", "aac", "alac"])),
        ("Archives".into(), c(&["zip", "rar", "7z", "tar", "gz", "xz", "zst", "bz2"])),
        ("Documents".into(), c(&["doc", "docx", "odt", "xls", "xlsx", "ods", "ppt", "pptx", "odp", "txt", "md", "epub"])),
        ("Logiciels".into(), c(&["appimage", "deb", "rpm", "flatpak", "flatpakref", "exe", "msi", "iso", "sh"])),
    ])
}

/// Téléchargements pas finis : on n'y touche jamais.
const PARTIAL: &[&str] = &["part", "crdownload", "tmp", "!qb", "download", "partial", "opdownload", "msp-part"];

fn ext_of(name: &str) -> String {
    Path::new(name).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

pub fn matches(rule: &Rule, cats: &BTreeMap<String, Vec<String>>, name: &str, size: u64) -> bool {
    let wanted: Vec<String> = rule
        .categories
        .iter()
        .flat_map(|c| cats.get(c).cloned().unwrap_or_default())
        .chain(rule.extensions.iter().cloned())
        .map(|e| e.trim().trim_start_matches('.').to_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    let by_ext = wanted.is_empty() || wanted.contains(&ext_of(name));
    let by_size = rule.min_size.is_none_or(|m| size > m);
    // Une règle sans aucun filtre ne prend rien.
    let has_filter = !wanted.is_empty() || rule.min_size.is_some();
    has_filter && by_ext && by_size
}

/// Décode les \040 & co. de /proc/self/mountinfo.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn mount_points() -> Vec<PathBuf> {
    fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split(' ').nth(4).map(|m| PathBuf::from(unescape(m))))
        .collect()
}

/// Le point de montage qui contient `path` (le plus long préfixe).
pub fn mount_of(path: &Path) -> PathBuf {
    let p = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    mount_points().into_iter().filter(|m| p.starts_with(m)).max_by_key(|m| m.as_os_str().len()).unwrap_or_else(|| "/".into())
}

/// Monté ET le dossier est là. Ne crée jamais rien : un dossier recréé sous
/// /run/media/... finirait sur le SSD.
pub fn target_ready(rule: &Rule) -> bool {
    mount_points().contains(&rule.mount) && rule.target.is_dir()
}

fn same_content(a: &Path, b: &Path) -> io::Result<bool> {
    let (mut fa, mut fb) = (fs::File::open(a)?, fs::File::open(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    loop {
        let n = fa.read(&mut ba)?;
        if n == 0 {
            return Ok(true);
        }
        fb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

/// `nom.ext`, sinon `nom (1).ext`, `nom (2).ext`…
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let mut dest = dir.join(name);
    let mut n = 1;
    while dest.symlink_metadata().is_ok() {
        dest = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    dest
}

/// Copie → vérification → suppression de l'original → lien. Jamais l'inverse.
pub fn move_and_link(src: &Path, target_dir: &Path) -> io::Result<PathBuf> {
    let name = src.file_name().ok_or(io::ErrorKind::InvalidInput)?.to_string_lossy().into_owned();
    let dest = free_name(target_dir, &name);
    let tmp = target_dir.join(format!(".{name}.msp-part"));
    let result = (|| {
        fs::copy(src, &tmp)?;
        let f = fs::File::options().write(true).open(&tmp)?;
        if let Ok(m) = fs::metadata(src).and_then(|m| m.modified()) {
            let _ = f.set_modified(m);
        }
        f.sync_all()?;
        if !same_content(src, &tmp)? {
            return Err(io::Error::other("la copie ne correspond pas à l'original"));
        }
        fs::rename(&tmp, &dest)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // Original impossible à enlever : on retire la copie, sinon chaque tour en
    // referait une nouvelle (doc (1), doc (2)…) jusqu'à remplir la cible.
    if let Err(e) = fs::remove_file(src) {
        let _ = fs::remove_file(&dest);
        return Err(e);
    }
    std::os::unix::fs::symlink(&dest, src)?;
    Ok(dest)
}

/// Fichiers ordinaires, finis, non cachés, du dossier (pas les sous-dossiers).
fn candidates(dir: &Path) -> Vec<(PathBuf, String, fs::Metadata)> {
    let Ok(rd) = fs::read_dir(dir) else { return vec![] };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.path().symlink_metadata().ok()?;
            let partial = PARTIAL.contains(&ext_of(&name).as_str());
            (meta.is_file() && !name.starts_with('.') && !partial).then(|| (e.path(), name, meta))
        })
        .collect()
}

/// Fichiers déjà présents qui correspondraient à la règle (aperçu « appliquer à l'existant »).
pub fn preview_existing(rule: &Rule, cats: &BTreeMap<String, Vec<String>>) -> Vec<PathBuf> {
    candidates(&rule.source).into_iter().filter(|(_, n, m)| matches(rule, cats, n, m.len())).map(|(p, ..)| p).collect()
}

fn trash(path: &Path) -> io::Result<()> {
    // gio suit la spec freedesktop : la corbeille du disque cible (.Trash-1000).
    let ok = std::process::Command::new("gio").arg("trash").arg(path).status()?.success();
    if ok { Ok(()) } else { Err(io::Error::other(format!("gio trash a échoué pour {}", path.display()))) }
}

/// Un lien vers `target` dans `dir`, en descendant jusqu'à `depth` niveaux.
/// Les dossiers cachés (dont la corbeille ~/.local/share/Trash) sont ignorés :
/// un lien mis à la corbeille compte bien comme supprimé.
fn find_link(dir: &Path, target: &Path, depth: u32) -> Option<PathBuf> {
    let mut subdirs = vec![];
    for e in fs::read_dir(dir).ok()?.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() && fs::read_link(e.path()).is_ok_and(|t| t == target) {
            return Some(e.path());
        }
        // file_type ne suit pas les liens : pas de boucle possible.
        if ft.is_dir() && depth > 0 && !e.file_name().to_string_lossy().starts_with('.') {
            subdirs.push(e.path());
        }
    }
    subdirs.iter().find_map(|d| find_link(d, target, depth - 1))
}

/// Où un lien déplacé a pu atterrir : le dossier perso, et le parent du départ
/// s'il est ailleurs (autre disque).
fn search_roots(source: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut roots = vec![home.clone()];
    let parent = source.parent().unwrap_or(source).to_path_buf();
    if home.as_os_str().is_empty() || !parent.starts_with(&home) {
        roots.push(parent);
    }
    roots
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[derive(Default)]
pub struct Engine {
    pub config: Config,
    pub links: Vec<Link>,
    /// Taille et mtime vus au tour précédent : un fichier ne part que stable.
    seen: HashMap<PathBuf, (u64, i64)>,
}

impl Engine {
    pub fn new(config: Config, links: Vec<Link>) -> Self {
        Self { config, links, seen: HashMap::new() }
    }

    /// Un tour : range les nouveaux fichiers, puis fait suivre les suppressions.
    /// Renvoie l'état et si `links` a changé (à sauvegarder).
    pub fn tick(&mut self) -> (Status, bool) {
        let mut status = Status::default();
        let before = self.links.clone();
        let ready: HashMap<String, bool> = self.config.rules.iter().map(|r| (r.id.clone(), target_ready(r))).collect();
        for r in self.config.rules.iter().filter(|r| r.enabled && !ready[&r.id]) {
            status.unplugged.push(r.id.clone());
        }

        let mut sources: Vec<&PathBuf> = self.config.rules.iter().filter(|r| r.enabled).map(|r| &r.source).collect();
        sources.sort();
        sources.dedup();
        let mut seen = HashMap::new();
        for dir in sources {
            for (path, name, meta) in candidates(dir) {
                // La première règle qui correspond gagne.
                let Some(rule) = self.config.rules.iter().find(|r| r.enabled && &r.source == dir && matches(r, &self.config.categories, &name, meta.len())) else {
                    continue;
                };
                if meta.ctime() < rule.created_at || !ready[&rule.id] {
                    continue;
                }
                let stamp = (meta.len(), meta.mtime());
                let stable = self.seen.get(&path) == Some(&stamp) && now() - meta.mtime() >= 2;
                seen.insert(path.clone(), stamp);
                if !stable {
                    continue;
                }
                match move_and_link(&path, &rule.target) {
                    Ok(dest) => self.links.push(Link { link: path, target: dest, rule: rule.id.clone() }),
                    Err(e) => status.last_error = Some(format!("{name} : {e}")),
                }
            }
        }
        self.seen = seen;
        self.reconcile(&ready, &mut status);
        let changed = self.links != before;
        (status, changed)
    }

    fn reconcile(&mut self, ready: &HashMap<String, bool>, status: &mut Status) {
        let rules = &self.config.rules;
        self.links.retain_mut(|l| {
            let Some(rule) = rules.iter().find(|r| r.id == l.rule) else { return false };
            if !rule.enabled || !ready.get(&rule.id).copied().unwrap_or(false) {
                return true; // en pause ou débranché : on ne conclut rien
            }
            let link_ok = fs::read_link(&l.link).is_ok_and(|t| t == l.target);
            let target_ok = l.target.exists();
            match (link_ok, target_ok) {
                (true, true) => true,
                (false, true) => {
                    // Lien renommé ou déplacé : on le suit au lieu de supprimer. On ne
                    // fouille tout le dossier perso que si une suppression est en jeu.
                    let dir = l.link.parent().unwrap_or(Path::new("/"));
                    let mut moved = find_link(dir, &l.target, 0);
                    if moved.is_none() && rule.delete_follows {
                        moved = search_roots(dir).iter().find_map(|r| find_link(r, &l.target, 8));
                    }
                    if let Some(p) = moved {
                        l.link = p;
                        return true;
                    }
                    if rule.delete_follows {
                        if let Err(e) = trash(&l.target) {
                            status.last_error = Some(e.to_string());
                            return true;
                        }
                    }
                    false
                }
                (true, false) => {
                    let _ = fs::remove_file(&l.link);
                    false
                }
                (false, false) => false,
            }
        });
    }

    /// Applique la règle aux fichiers déjà présents choisis dans l'aperçu.
    pub fn apply_existing(&mut self, rule_id: &str, files: &[PathBuf]) -> Result<usize, String> {
        let rule = self.config.rules.iter().find(|r| r.id == rule_id).ok_or("règle inconnue")?.clone();
        if !target_ready(&rule) {
            return Err("la cible est débranchée".into());
        }
        let mut n = 0;
        for f in files.iter().filter(|f| f.parent() == Some(rule.source.as_path())) {
            if f.symlink_metadata().is_ok_and(|m| m.is_file()) {
                let dest = move_and_link(f, &rule.target).map_err(|e| e.to_string())?;
                self.links.push(Link { link: f.clone(), target: dest, rule: rule.id.clone() });
                n += 1;
            }
        }
        Ok(n)
    }
}

/// Complète une règle neuve et refuse ce qui serait dangereux.
pub fn prepare_rule(mut r: Rule) -> Result<Rule, String> {
    // Une règle déjà enregistrée garde son départ même s'il est absent : sinon
    // un seul départ débranché empêcherait de toucher aux autres règles.
    match fs::canonicalize(&r.source) {
        Ok(p) => r.source = p,
        Err(_) if r.created_at != 0 => {}
        Err(_) => return Err("dossier de départ introuvable".into()),
    }
    if r.mount.as_os_str().is_empty() {
        r.target = fs::canonicalize(&r.target).map_err(|_| "dossier cible introuvable (disque branché ?)")?;
        r.mount = mount_of(&r.target);
    }
    if r.target == r.source {
        return Err("le départ et la cible sont le même dossier".into());
    }
    if r.created_at == 0 {
        r.created_at = now();
    }
    if r.id.is_empty() {
        r.id = format!("r{}{}", now(), std::process::id());
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("msp-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("src")).unwrap();
        fs::create_dir_all(d.join("dst")).unwrap();
        d
    }

    fn rule(d: &Path) -> Rule {
        prepare_rule(Rule {
            source: d.join("src"),
            target: d.join("dst"),
            categories: vec!["PDF".into()],
            enabled: true,
            created_at: 1,
            ..Default::default()
        })
        .unwrap()
    }

    fn engine(r: Rule) -> Engine {
        Engine::new(Config { rules: vec![r], categories: default_categories() }, vec![])
    }

    /// Deux tours suffisent quand le fichier a plus de 2 s.
    fn settle(e: &mut Engine) -> Status {
        e.tick();
        e.tick().0
    }

    fn old_file(p: &Path, body: &str) {
        fs::write(p, body).unwrap();
        let f = fs::File::options().write(true).open(p).unwrap();
        f.set_modified(SystemTime::now() - std::time::Duration::from_secs(10)).unwrap();
    }

    #[test]
    fn moves_matching_file_and_leaves_a_link() {
        let d = tmp("move");
        let mut e = engine(rule(&d));
        old_file(&d.join("src/doc.pdf"), "pdf!");
        old_file(&d.join("src/setup.AppImage"), "app");
        settle(&mut e);
        let link = d.join("src/doc.pdf");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), fs::canonicalize(d.join("dst/doc.pdf")).unwrap());
        assert_eq!(fs::read_to_string(&link).unwrap(), "pdf!");
        assert!(d.join("src/setup.AppImage").symlink_metadata().unwrap().is_file(), "le logiciel reste");
        assert_eq!(e.links.len(), 1);
    }

    #[test]
    fn skips_partial_unstable_and_existing() {
        let d = tmp("skip");
        let mut r = rule(&d);
        old_file(&d.join("src/vieux.pdf"), "x");
        r.created_at = now() + 100; // tout ce qui est là est « déjà présent »
        let mut e = engine(r);
        old_file(&d.join("src/a.pdf.part"), "x");
        settle(&mut e);
        assert!(d.join("src/vieux.pdf").symlink_metadata().unwrap().is_file());
        assert!(d.join("src/a.pdf.part").symlink_metadata().unwrap().is_file());

        let d = tmp("fresh");
        let mut e = engine(rule(&d));
        fs::write(d.join("src/frais.pdf"), "x").unwrap(); // mtime = maintenant
        settle(&mut e);
        assert!(d.join("src/frais.pdf").symlink_metadata().unwrap().is_file(), "pas encore stable");
    }

    #[test]
    fn unplugged_target_keeps_file_and_never_creates_target() {
        let d = tmp("unplug");
        let mut r = rule(&d);
        r.mount = "/n-existe-pas".into();
        let mut e = engine(r.clone());
        old_file(&d.join("src/doc.pdf"), "x");
        let s = settle(&mut e);
        assert_eq!(s.unplugged, vec![r.id.clone()]);
        assert!(d.join("src/doc.pdf").symlink_metadata().unwrap().is_file());

        fs::remove_dir(d.join("dst")).unwrap();
        e.config.rules[0].mount = mount_of(&d);
        settle(&mut e);
        assert!(!d.join("dst").exists(), "la cible n'est jamais recréée");
        assert!(d.join("src/doc.pdf").symlink_metadata().unwrap().is_file());

        fs::create_dir(d.join("dst")).unwrap(); // rebranché
        settle(&mut e);
        assert!(d.join("src/doc.pdf").symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[test]
    fn name_clash_gets_a_suffix() {
        let d = tmp("clash");
        fs::write(d.join("dst/doc.pdf"), "ancien").unwrap();
        let mut e = engine(rule(&d));
        old_file(&d.join("src/doc.pdf"), "nouveau");
        settle(&mut e);
        assert_eq!(fs::read_to_string(d.join("dst/doc (1).pdf")).unwrap(), "nouveau");
        assert_eq!(fs::read_to_string(d.join("dst/doc.pdf")).unwrap(), "ancien");
    }

    #[test]
    fn deleting_target_removes_link_and_renamed_link_is_followed() {
        let d = tmp("follow");
        let mut e = engine(rule(&d));
        old_file(&d.join("src/a.pdf"), "a");
        old_file(&d.join("src/b.pdf"), "b");
        settle(&mut e);
        fs::remove_file(d.join("dst/a.pdf")).unwrap();
        fs::rename(d.join("src/b.pdf"), d.join("src/b2.pdf")).unwrap();
        e.tick();
        assert!(d.join("src/a.pdf").symlink_metadata().is_err(), "lien cassé enlevé");
        assert!(d.join("dst/b.pdf").exists(), "lien renommé : la cible reste");
        assert_eq!(e.links.len(), 1);
        assert!(e.links[0].link.ends_with("b2.pdf"));
    }

    #[test]
    fn deleting_link_without_option_keeps_target() {
        let d = tmp("keep");
        let mut e = engine(rule(&d));
        old_file(&d.join("src/a.pdf"), "a");
        settle(&mut e);
        fs::remove_file(d.join("src/a.pdf")).unwrap();
        e.tick();
        assert!(d.join("dst/a.pdf").exists());
        assert!(e.links.is_empty());
    }

    #[test]
    fn delete_follows_spares_a_moved_link() {
        let d = tmp("moved");
        let mut r = rule(&d);
        r.delete_follows = true;
        let mut e = engine(r);
        old_file(&d.join("src/a.pdf"), "a");
        settle(&mut e);
        fs::create_dir_all(d.join("ailleurs/sous")).unwrap();
        fs::rename(d.join("src/a.pdf"), d.join("ailleurs/sous/a.pdf")).unwrap();
        e.tick();
        assert!(d.join("dst/a.pdf").exists(), "lien déplacé : le vrai fichier reste");
        assert!(e.links[0].link.ends_with("ailleurs/sous/a.pdf"), "le lien est suivi");

        // Mis à la corbeille (dossier caché) : là, c'est une vraie suppression.
        fs::create_dir_all(d.join(".Trash/files")).unwrap();
        fs::rename(d.join("ailleurs/sous/a.pdf"), d.join(".Trash/files/a.pdf")).unwrap();
        let (s, _) = e.tick();
        // /tmp (tmpfs) n'a pas de corbeille : gio refuse, mais la mise à la corbeille a bien été tentée.
        let tried = s.last_error.is_some_and(|m| m.contains("gio trash"));
        assert!(tried || !d.join("dst/a.pdf").exists(), "le vrai fichier part à la corbeille");
    }

    #[test]
    fn undeletable_original_leaves_no_copy_behind() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("ro");
        let mut e = engine(rule(&d));
        old_file(&d.join("src/doc.pdf"), "x");
        fs::set_permissions(d.join("src"), fs::Permissions::from_mode(0o555)).unwrap();
        let s = settle(&mut e);
        e.tick();
        fs::set_permissions(d.join("src"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(s.last_error.is_some());
        assert_eq!(fs::read_dir(d.join("dst")).unwrap().count(), 0, "pas de copies en double");
        assert!(d.join("src/doc.pdf").symlink_metadata().unwrap().is_file());
    }

    #[test]
    fn filters() {
        let cats = default_categories();
        let mut r = Rule { categories: vec!["Images".into()], ..Default::default() };
        assert!(matches(&r, &cats, "Photo.JPG", 1));
        assert!(!matches(&r, &cats, "a.pdf", 1));
        r.min_size = Some(100);
        assert!(!matches(&r, &cats, "p.png", 50));
        assert!(matches(&r, &cats, "p.png", 500));
        let size_only = Rule { min_size: Some(10), ..Default::default() };
        assert!(matches(&size_only, &cats, "x.bin", 11));
        assert!(!matches(&Rule::default(), &cats, "x.pdf", 1), "sans filtre, rien");
        let free = Rule { extensions: vec![".STL".into()], ..Default::default() };
        assert!(matches(&free, &cats, "piece.stl", 1));
        assert_eq!(unescape("/run/media/a\\040b"), "/run/media/a b");
    }
}
