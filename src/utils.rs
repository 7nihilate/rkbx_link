use rekordcrate::anlz::Phrase;

pub struct PhraseParser {
    phrase_names: Vec<Vec<String>>,
    hi_phrase_names: Vec<Vec<String>>,
}

impl PhraseParser {
    pub fn new() -> Self {
        Self {
            phrase_names: vec![
                ["Intro", "Verse 1", "Verse 1", "Verse 1", "Verse 2", "Verse 2", "Verse 2", "Bridge", "Chorus", "Outro"].iter().map(|s| s.to_string()).collect(),
                ["Intro", "Verse 1", "Verse 2", "Verse 3", "Verse 4", "Verse 5", "Verse 6", "Bridge", "Chorus", "Outro"].iter().map(|s| s.to_string()).collect(),
                // ["Intro", "Up", "Down", "-", "Chorus", "Outro", "-", "-", "-"].iter().map(|s| s.to_string()).collect(),
                ["Intro 1", "Intro 2", "Up 1", "Up 2", "Up 3", "Down", "Chorus 1", "Chorus 2", "Outro 1", "Outro 2"].iter().map(|s| s.to_string()).collect(),
            ],
            hi_phrase_names: [
                vec!["Intro 2", "Intro 1"],
                vec!["Up 1", "Up 2", "Up 3"],
                vec!["Down"],
                vec!["Chorus 2", "Chorus 1"],
                vec!["Outro 2", "Outro 1"],
            ].iter().map(|x| x.iter().map(|s| s.to_string()).collect()).collect(),
        }
    }

    /// The Rekordbox name of a phrase, or "" for a kind this table does not know. Must never
    /// panic: it runs on every update, and a panic here stopped all output in the middle of a
    /// song (a high-mood phrase whose k1/k2/k3 variant is past the end of its row, or kind 0).
    pub fn get_phrase_name(&self, mood: &rekordcrate::anlz::Mood, phrase: &Phrase) -> String {
        if mood == &rekordcrate::anlz::Mood::High {
            let row = match phrase.kind {
                1 => 0,
                2 => 1,
                3 => 2,
                5 => 3,
                6 => 4,
                _ => return String::new(),
            };
            let names = &self.hi_phrase_names[row];
            let variant = phrase.k1 as usize + 2 * phrase.k2 as usize + phrase.k3 as usize;
            // A variant past the row (k flags combined, e.g. Up with k2 and k3) keeps the phrase kind
            // and takes the row's last name ("Up 3", "Down"...)
            return names.get(variant).or(names.last()).cloned().unwrap_or_default();
        }
        let Some(index) = (phrase.kind as usize).checked_sub(1) else {
            return String::new();
        };
        self.phrase_names[self.mood_to_int(mood)].get(index).cloned().unwrap_or_default()
    }

    fn mood_to_int(&self, mood: &rekordcrate::anlz::Mood) -> usize {
        match mood {
            rekordcrate::anlz::Mood::Low => 0,
            rekordcrate::anlz::Mood::Mid => 1,
            rekordcrate::anlz::Mood::High => 2,
        }
    }

    pub fn phrase_name_to_index (phrase_name: &str) -> i32{
        match phrase_name {
            "Intro" | "Intro 1" | "Intro 2" => 1,
            "Verse 1" | "Verse 2" | "Verse 3" | "Verse 4" | "Verse 5" | "Verse 6" | "Up 1" | "Up 2" | "Up 3" => 2,
            "Chorus" | "Chorus 1" | "Chorus 2" => 3,
            "Bridge" | "Down" => 4,
            "Outro" | "Outro 1" | "Outro 2" => 5,
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use binrw::BinRead;
    use rekordcrate::anlz::Mood;

    /// A 24-byte Phrase as stored in the EXT file (big endian).
    fn phrase(kind: u16, k1: u8, k2: u8, k3: u8) -> Phrase {
        let mut b = vec![0u8; 24];
        b[0..2].copy_from_slice(&1u16.to_be_bytes());
        b[2..4].copy_from_slice(&1u16.to_be_bytes());
        b[4..6].copy_from_slice(&kind.to_be_bytes());
        b[7] = k1;
        b[9] = k2;
        b[19] = k3;
        Phrase::read_be(&mut std::io::Cursor::new(b)).unwrap()
    }

    /// The lookup as it was: panicked on these phrases, which stopped all output mid-song.
    fn old_get_phrase_name(p: &PhraseParser, mood: &Mood, phrase: &Phrase) -> String {
        if mood == &Mood::High {
            let phrase_kind = match phrase.kind { 1 => 0, 2 => 1, 3 => 2, 5 => 3, 6 => 4, _ => 99 };
            return p.hi_phrase_names[phrase_kind][(phrase.k1 + 2 * phrase.k2 + phrase.k3) as usize].clone();
        }
        p.phrase_names[p.mood_to_int(mood)][phrase.kind as usize - 1].clone()
    }

    #[test]
    fn old_lookup_panicked_on_these() {
        let p = PhraseParser::new();
        for (mood, ph) in [(Mood::High, phrase(3, 1, 0, 0)), (Mood::High, phrase(2, 1, 1, 0)),
                           (Mood::High, phrase(4, 0, 0, 0)), (Mood::Low, phrase(0, 0, 0, 0)),
                           (Mood::Mid, phrase(11, 0, 0, 0))] {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| old_get_phrase_name(&p, &mood, &ph)));
            assert!(r.is_err(), "expected the old lookup to panic for {mood:?} kind {}", ph.kind);
        }
    }

    #[test]
    fn lookup_never_panics() {
        let p = PhraseParser::new();
        assert_eq!(p.get_phrase_name(&Mood::High, &phrase(3, 1, 0, 0)), "Down");
        assert_eq!(p.get_phrase_name(&Mood::High, &phrase(2, 1, 1, 0)), "Up 3");
        assert_eq!(p.get_phrase_name(&Mood::High, &phrase(4, 0, 0, 0)), "");
        assert_eq!(p.get_phrase_name(&Mood::Low, &phrase(0, 0, 0, 0)), "");
        assert_eq!(p.get_phrase_name(&Mood::Mid, &phrase(11, 0, 0, 0)), "");
        assert_eq!(p.get_phrase_name(&Mood::High, &phrase(1, 1, 0, 0)), "Intro 1");
        assert_eq!(p.get_phrase_name(&Mood::Mid, &phrase(9, 0, 0, 0)), "Chorus");
        for mood in [Mood::Low, Mood::Mid, Mood::High] {
            for kind in 0..12u16 {
                for k in 0..2u8 {
                    p.get_phrase_name(&mood, &phrase(kind, k, k, k));
                }
            }
        }
    }

    /// Scans a Rekordbox analysis folder (RKBX_ANLZ_DIR, the PIONEER/USBANLZ folder) for tracks
    /// whose phrases crashed the old lookup. `cargo test --release scan_library -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn scan_library() {
        let Ok(root) = std::env::var("RKBX_ANLZ_DIR") else { return };
        std::panic::set_hook(Box::new(|_| {}));
        let p = PhraseParser::new();
        let (mut files, mut bad) = (0, 0);
        let mut stack = vec![std::path::PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() { stack.push(path); continue; }
                if path.extension().is_none_or(|e| e != "EXT") { continue; }
                let Ok(bytes) = std::fs::read(&path) else { continue };
                let Ok(anlz) = rekordcrate::anlz::ANLZ::read(&mut std::io::Cursor::new(bytes)) else { continue };
                for section in anlz.sections {
                    let rekordcrate::anlz::Content::SongStructure(ss) = section.content else { continue };
                    files += 1;
                    let crashers: Vec<String> = ss.data.phrases.iter().filter(|ph| {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| old_get_phrase_name(&p, &ss.data.mood, ph))).is_err()
                    }).map(|ph| format!("kind {} k {}{}{} at beat {}", ph.kind, ph.k1, ph.k2, ph.k3, ph.beat)).collect();
                    if !crashers.is_empty() {
                        bad += 1;
                        let dat = std::fs::read(path.with_extension("DAT")).ok()
                            .and_then(|b| rekordcrate::anlz::ANLZ::read(&mut std::io::Cursor::new(b)).ok())
                            .and_then(|a| a.sections.into_iter().find_map(|s| match s.content {
                                rekordcrate::anlz::Content::Path(p) => Some(p.path.to_string()), _ => None }))
                            .unwrap_or_default();
                        if bad <= 25 {
                            println!("CRASH {:?} {} | {}", ss.data.mood, dat.rsplit(['/', '\\']).next().unwrap_or(""), crashers.join(", "));
                        }
                    }
                }
            }
        }
        println!("SCAN {files} tracks with phrases, {bad} would have stopped rkbx_link");
    }

    /// Prints the phrases of the tracks whose audio path contains RKBX_DUMP (with RKBX_ANLZ_DIR).
    #[test]
    #[ignore]
    fn dump_track() {
        let (Ok(root), Ok(wanted)) = (std::env::var("RKBX_ANLZ_DIR"), std::env::var("RKBX_DUMP")) else { return };
        let p = PhraseParser::new();
        let read = |path: &std::path::Path| std::fs::read(path).ok()
            .and_then(|b| rekordcrate::anlz::ANLZ::read(&mut std::io::Cursor::new(b)).ok());
        let mut stack = vec![std::path::PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() { stack.push(path); continue; }
                if path.extension().is_none_or(|e| e != "DAT") { continue; }
                let Some(audio) = read(&path).and_then(|a| a.sections.into_iter().find_map(|s| match s.content {
                    rekordcrate::anlz::Content::Path(p) => Some(p.path.to_string()), _ => None })) else { continue };
                if !audio.to_lowercase().contains(&wanted.to_lowercase()) { continue; }
                let Some(ext) = read(&path.with_extension("EXT")) else { continue };
                for section in ext.sections {
                    let rekordcrate::anlz::Content::SongStructure(ss) = section.content else { continue };
                    println!("TRACK {audio} mood {:?}", ss.data.mood);
                    for ph in &ss.data.phrases {
                        println!("  beat {:4} kind {} k {}{}{} -> {}", ph.beat, ph.kind, ph.k1, ph.k2, ph.k3,
                                 p.get_phrase_name(&ss.data.mood, ph));
                    }
                }
            }
        }
    }
}
