// frecent_paths is a specialization of frecency that understands the semantics of stored paths.
// It does things like the messyness of checking for a directory's existence and such.

use frecency::Frecency;
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::fs;
use serde::Serialize;
use serde;
use libc;
use rmp_serde;
use matcher::*;

pub struct PathFrecency {
    frecency: Frecency<String>,
    // whether the frecency file is 'dirty' and should be updated on save
    dirty: bool,
    path: PathBuf,
}

impl PathFrecency {
    // load loads or, if it doesn't exist, creates a path frecency db at a given location
    pub fn load(path: &Path) -> Self {
        let frecency_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .unwrap();
        let metadata = frecency_file.metadata().unwrap();
        // remember 1000 entries total
        let frecency = if metadata.len() > 0 {
            // existing file, unmarshal that sucker
            let mut de = rmp_serde::Deserializer::from_read(frecency_file);
            serde::Deserialize::deserialize(&mut de).unwrap()
        } else {
            Frecency::<String>::new(1000)
        };

        PathFrecency {
            frecency: frecency,
            path: path.to_path_buf(),
            dirty: false,
        }
    }

    pub fn visit(&mut self, dir: String) {
        self.frecency.visit(dir);
        self.dirty = true
    }

    pub fn maybe_add_relative_to(&mut self, mut base_path: PathBuf, relative_path: &str) -> bool {
        // If the path exists, add it to the database
        base_path.push(relative_path);
        if base_path.is_dir() {
            base_path
                .to_str()
                .map(|base_path_str| {
                    debug!("Visited path exists: {}", base_path_str);
                    self.frecency.insert(base_path_str.to_owned());
                    self.dirty = true;
                    true
                })
                .unwrap_or(false)
        } else {
            false
        }
    }

    pub fn save_to_disk(&self) -> Result<(), String> {
        if !self.dirty {
            // No need to save, nothing's changed
            return Ok(());
        }
        // Transform frecency path into a temporary path for atomic move
        let my_pid = unsafe { libc::getpid() };
        if my_pid == 0 {
            return Err("could not get pid".to_string());
        }

        let fname = self.path
            .file_name()
            .ok_or("path did not have file component".to_string())?;

        let tmpfile_name = format!(".{}.{}", fname.to_string_lossy(), my_pid);
        let tmpfile_dir = self.path
            .parent()
            .ok_or("unable to get parent".to_string())?;
        let tmpfile_path = tmpfile_dir.join(tmpfile_name);

        let tmpfile =
            fs::File::create(&tmpfile_path).map_err(|_| "could not create tempfile".to_string())?;

        self.frecency
            .serialize(&mut rmp_serde::Serializer::new(tmpfile))
            .map_err(|_| "could not create tmpfile".to_string())?;
        fs::rename(tmpfile_path, &self.path)
            .map_err(|e| format!("could not atomically rename: {}", e).to_string())
    }

    pub fn items_with_frecency(&mut self) -> Vec<(&String, f64)> {
        if self.frecency.retain(|path| {
            if Path::new(path).is_dir() {
                true
            } else {
                debug!("trimming nonexistent dir: {}", path);
                false
            }
        }) {
            self.dirty = true;
        }
        let mut items = self.frecency.normalized_frecency();
        items.sort_by(|lhs, rhs| {
            // NaN shouldn't happen
            lhs.1
                .partial_cmp(&rhs.1)
                .expect(&format!("{} could not be compared to {}", lhs.1, rhs.1))
        });

        items
    }

    pub fn directory_matches(&mut self, filter: &str) -> Vec<(&String, f64)> {
        // 'best directory' is a tricky concept, as is 'match.
        //
        // There's a continuum from "exact string match" to "no characters in common", and we
        // have to try and approximate what a human expects to figure out the weight and cutoff
        // within that continuum.
        //
        // The following assumptions are what I started with:
        // 1) Exact matches should always be jumped to with no questions asked. Exact matches are
        //    rare. Substring matches are permissible and expected.
        // 2) Components should be deconstructed from frecency database items for matching; people
        //    think in components. For example, an entry of "/home/user/dev" will be thought about
        //    by a user as the three distinct components "home", "user", and "dev", so we can
        //    better match their expectations by matching individual components.
        // 3) Component matches should be weighted based on how "deep" / "far right" the matched
        //    component is. That is to say, the query "foo" should be weighted more highly for
        //    "/home/user/project/foo" than for "/home/user/foo/stuff", even if the latter is
        //    higher in the frecency index.
        // 4) Case and punctuation in the target are liable to not be present in the query.
        // 5) If the query contains a component separator, the user likely wants each side of it to
        //    be fuzzy. That is to say: "z dev/tool" likely wishes to do a fuzzy match on the
        //    strings "dev" and "tool" on adjacent components, leading to results like
        //    "dev/my-tool" being possible.
        // 6) Levenshtein distance may be fallen back upon for real "fuzzyness", but should be
        //    weighted carefully low; sometimes it is better to force a user to make a new query
        //    than to make too strange of a shot in the dark.

        let em = ExactMatcher {};
        let sm = SubstringMatcher {};
        let ci_em = CaseInsensitiveMatcher::new(&em);
        let pc_em = PathComponentMatcher::new(&em);
        let pc_sm = PathComponentMatcher::new(&sm);
        let pc_ci_em = PathComponentMatcher::new(&ci_em);
        let ci_sm = CaseInsensitiveMatcher::new(&sm);
        let pc_ci_sm = PathComponentMatcher::new(&ci_sm);
        let matchers: Vec<&Matcher> = vec![
            &ExactMatcher {},
            &ci_em,
            &pc_em,
            &pc_sm,
            &pc_ci_em,
            &SubstringMatcher {},
            &ci_sm,
            &pc_ci_sm,
        ];

        let matched = self.items_with_frecency()
            .iter()
            .flat_map(|item| {
                matchers
                    .iter()
                    .filter_map(move |m| match m.matches(item.0, filter) {
                        Some(v) => Some((item.0, v * 0.8 + item.1 * 0.2)),
                        None => None,
                    })
            })
            .collect::<Vec<_>>();

        // Remove dupe paths, keeping only each with the highest score
        let mut dedupe_map: HashMap<&String, f64> = HashMap::new();
        for el in &matched {
            match dedupe_map.get(el.0) {
                Some(&val) => {
                    if el.1 > val {
                        dedupe_map.insert(el.0, el.1);
                    }
                }
                None => {
                    dedupe_map.insert(el.0, el.1);
                }
            }
        }

        let mut deduped = dedupe_map.into_iter().collect::<Vec<_>>();
        deduped.sort_by(|lhs, rhs| {
            // NaN shouldn't happen
            lhs.1
                .partial_cmp(&rhs.1)
                .expect(&format!("{} could not be compared to {}", lhs.1, rhs.1))
        });

        debug!(
            "{}",
            deduped
                .iter()
                .fold("Matched paths:".to_string(), |acc, el| acc
                    + &format!("\n{} with score {}", el.0, el.1))
        );

        deduped
    }
}
