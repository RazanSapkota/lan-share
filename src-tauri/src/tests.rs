//! One test module, plain `#[test] fn`s, same shape as the reference app.
//!
//! The path-traversal block is the priority: it is the only part of this app
//! where a bug hands a stranger on the Wi-Fi the contents of `C:\`.

use std::{fs, path::PathBuf};

use crate::{
    auth, config, media,
    models::{AppConfig, Peer, ResolvedShare, ServerSettings, Share, ShareRegistry},
    net, shares,
    shares::PathReject,
    utils,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Unique temp directory per test. A plain counter plus the process id keeps
/// parallel test threads (and repeat runs) from colliding without needing a
/// tempfile dependency.
fn tempdir_unique(suffix: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "lanshare_test_{}_{}_{}",
        std::process::id(),
        n,
        suffix
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

fn write_file(path: &std::path::Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// A share rooted at `root`, canonicalized the same way the real code does.
fn share_at(root: &std::path::Path, name: &str) -> ResolvedShare {
    let canonical = shares::canonical_root(&root.to_string_lossy()).unwrap();
    ResolvedShare {
        cfg: Share {
            id: "test".to_string(),
            name: name.to_string(),
            path: root.to_string_lossy().to_string(),
            token: "TESTTOKEN".to_string(),
            enabled: true,
            is_inbox: false,
            read_only: true,
            is_file: false,
            recursive: true,
            include_names: Vec::new(),
            include_ext: Vec::new(),
            exclude_ext: Vec::new(),
            added_ms: 0,
            note: None,
        },
        root: canonical,
        root_exists: true,
        expires_ms: None,
    }
}

fn default_settings() -> ServerSettings {
    ServerSettings::from_config(&AppConfig::default())
}

// ===========================================================================
// Path traversal -- the critical block
// ===========================================================================

/// Every one of these must be refused. Grouped so a failure names the family.
#[test]
fn split_rel_rejects_dot_dot_forms() {
    for input in [
        "..",
        "../",
        "../..",
        "a/../../b",
        "../etc/passwd",
        "..\\..\\Windows",
        "a/..",
        "foo/../../bar",
    ] {
        assert_eq!(
            shares::split_rel(input),
            Err(PathReject::Traversal),
            "expected traversal rejection for {input:?}"
        );
    }
}

#[test]
fn split_rel_rejects_single_dot_segments() {
    for input in ["./a", "a/./b", "."] {
        assert_eq!(
            shares::split_rel(input),
            Err(PathReject::DotSegment),
            "expected dot-segment rejection for {input:?}"
        );
    }
}

/// `....//` collapses to `..` under any implementation that strips rather than
/// rejects. Ours treats `....` as a plain (if odd) segment name and rejects it
/// only for the trailing dot -- either way it never becomes traversal.
#[test]
fn split_rel_rejects_quad_dot_trick() {
    assert_eq!(
        shares::split_rel("....//"),
        Err(PathReject::TrailingDotOrSpace)
    );
    assert_eq!(
        shares::split_rel("....//etc/passwd"),
        Err(PathReject::TrailingDotOrSpace)
    );
}

/// These arrive already percent-decoded from axum. The raw (still-encoded)
/// forms must ALSO be refused, because a second decode is exactly the bug we
/// are defending against -- `%2e%2e%2f` must never be treated as `../`.
#[test]
fn split_rel_treats_encoded_forms_literally() {
    // Still-encoded input: '%' is a legal filename character, so these are
    // valid segments that simply will not exist on disk. The critical property
    // is that they are NOT decoded into traversal.
    assert_eq!(shares::split_rel("%2e%2e%2fetc").unwrap(), vec!["%2e%2e%2fetc"]);
    assert_eq!(shares::split_rel("%252e%252e").unwrap(), vec!["%252e%252e"]);
    // Once decoded (what axum hands us), they must be rejected.
    assert_eq!(shares::split_rel("../etc"), Err(PathReject::Traversal));
}

#[test]
fn split_rel_rejects_absolute_and_rooted_forms() {
    for input in [
        "/etc/passwd",
        "/",
        "\\Windows\\System32",
        "\\",
        "C:\\Windows",
        "c:/windows",
        "C:foo",
        "c:",
        "D:\\media\\x",
        "\\\\?\\C:\\Windows",
        "\\\\.\\PhysicalDrive0",
        "\\\\server\\share\\x",
        "//server/share/x",
    ] {
        assert_eq!(
            shares::split_rel(input),
            Err(PathReject::Absolute),
            "expected absolute rejection for {input:?}"
        );
    }
}

/// A colon is both the NTFS Alternate Data Stream separator and the
/// drive-relative separator. Rejected on every platform so the tests and the
/// behaviour are identical everywhere.
#[test]
fn check_segment_rejects_colon_and_alternate_data_streams() {
    for input in [
        "file.txt:stream",
        "file.txt:$DATA",
        "notes.txt:hidden:$DATA",
        "a:b",
    ] {
        assert_eq!(
            shares::check_segment(input),
            Err(PathReject::Colon),
            "expected colon rejection for {input:?}"
        );
    }
}

#[test]
fn check_segment_rejects_windows_reserved_devices() {
    for input in [
        "CON", "con", "con.txt", "NUL", "nul.jpg", "COM1", "COM1.tar.gz", "LPT9.txt", "CONIN$",
        "CONOUT$", "PRN", "AUX",
    ] {
        assert!(
            matches!(
                shares::check_segment(input),
                Err(PathReject::ReservedDevice(_))
            ),
            "expected reserved-device rejection for {input:?}"
        );
    }
}

/// `aux.` and `NUL   ` trip the trailing-dot/space rule first. Both rules
/// reject; this test pins down which, so a refactor that drops one is caught.
#[test]
fn trailing_dot_and_space_are_rejected_before_device_check() {
    assert_eq!(
        shares::check_segment("aux."),
        Err(PathReject::TrailingDotOrSpace)
    );
    assert_eq!(
        shares::check_segment("NUL   "),
        Err(PathReject::TrailingDotOrSpace)
    );
    // But the device check still catches them via is_reserved_device directly.
    assert!(shares::is_reserved_device("aux."));
    assert!(shares::is_reserved_device("NUL   "));
    assert!(shares::is_reserved_device("COM1.tar.gz"));
    assert!(!shares::is_reserved_device("console.txt"));
    assert!(!shares::is_reserved_device("nullable"));
}

/// Windows silently strips these, so `secret.txt.` and `secret.txt` would name
/// one file through two URLs -- defeating any name-based rule and splitting the
/// thumbnail cache.
#[test]
fn check_segment_rejects_trailing_dot_or_space() {
    for input in ["trailing ", "trailing.", "trailing. ", "trailing .."] {
        assert_eq!(
            shares::check_segment(input),
            Err(PathReject::TrailingDotOrSpace),
            "expected trailing dot/space rejection for {input:?}"
        );
    }
}

#[test]
fn check_segment_rejects_control_characters() {
    for ch in ['\0', '\x01', '\x1f', '\x7f'] {
        let input = format!("file{ch}name");
        assert!(
            matches!(
                shares::check_segment(&input),
                Err(PathReject::IllegalChar(_))
            ),
            "expected control-character rejection for {ch:?}"
        );
    }
}

#[test]
fn check_segment_rejects_windows_illegal_characters() {
    for ch in ['<', '>', '"', '|', '?', '*'] {
        let input = format!("file{ch}name");
        assert_eq!(
            shares::check_segment(&input),
            Err(PathReject::IllegalChar(ch)),
            "expected illegal-character rejection for {ch:?}"
        );
    }
}

#[test]
fn check_segment_rejects_separators() {
    assert!(matches!(
        shares::check_segment("a/b"),
        Err(PathReject::IllegalChar('/'))
    ));
    assert!(matches!(
        shares::check_segment("a\\b"),
        Err(PathReject::IllegalChar('\\'))
    ));
}

#[test]
fn check_segment_rejects_overlong_segment() {
    let long = "a".repeat(256);
    assert_eq!(
        shares::check_segment(&long),
        Err(PathReject::SegmentTooLong)
    );
    let ok = "a".repeat(255);
    assert!(shares::check_segment(&ok).is_ok());
}

#[test]
fn split_rel_accepts_and_collapses_benign_paths() {
    assert_eq!(shares::split_rel("").unwrap(), Vec::<&str>::new());
    assert_eq!(shares::split_rel("a").unwrap(), vec!["a"]);
    assert_eq!(shares::split_rel("a/b/c").unwrap(), vec!["a", "b", "c"]);
    // Repeated and trailing separators collapse rather than producing empty
    // segments that could confuse a later join.
    assert_eq!(shares::split_rel("a//b").unwrap(), vec!["a", "b"]);
    assert_eq!(shares::split_rel("a/b/").unwrap(), vec!["a", "b"]);
    // Backslash is a separator on every platform -- see the note in split_rel.
    assert_eq!(shares::split_rel("a\\b").unwrap(), vec!["a", "b"]);
}

#[test]
fn split_rel_accepts_unicode_names() {
    assert_eq!(shares::split_rel("café/🎬.mp4").unwrap(), vec!["café", "🎬.mp4"]);
    // An RTL override renders deceptively but is not a traversal vector, and
    // rejecting it would break legitimate Arabic and Hebrew filenames.
    assert_eq!(shares::split_rel("a\u{202E}b").unwrap(), vec!["a\u{202E}b"]);
    // NFC and NFD forms are distinct byte sequences and both are legal names.
    assert!(shares::split_rel("e\u{0301}clair").is_ok());
    assert!(shares::split_rel("éclair").is_ok());
}

#[test]
fn normalize_rel_produces_forward_slash_form() {
    assert_eq!(shares::normalize_rel("a\\b\\c").unwrap(), "a/b/c");
    assert_eq!(shares::normalize_rel("a//b/").unwrap(), "a/b");
    assert_eq!(shares::normalize_rel("").unwrap(), "");
}

// --- containment ------------------------------------------------------------

/// The classic bug: a naive string `starts_with` lets root `D:\media` contain
/// `D:\media-private\secret.txt`. `Path::starts_with` is component-wise and
/// rejects it -- this test locks that in.
#[test]
fn contained_in_rejects_sibling_prefix() {
    let root = PathBuf::from("/data/media");
    let sibling = PathBuf::from("/data/media-private/secret.txt");
    assert!(!shares::contained_in(&root, &sibling));

    let child = PathBuf::from("/data/media/photo.jpg");
    assert!(shares::contained_in(&root, &child));
}

#[test]
fn contained_in_accepts_the_root_itself() {
    let root = PathBuf::from("/data/media");
    assert!(shares::contained_in(&root, &root));
}

#[cfg(windows)]
#[test]
fn contained_in_is_case_insensitive_on_windows() {
    let root = PathBuf::from(r"D:\Media");
    let child = PathBuf::from(r"d:\media\Photo.JPG");
    assert!(shares::contained_in(&root, &child));
}

// --- resolve_within against a real directory --------------------------------

#[test]
fn resolve_within_resolves_real_files_and_refuses_escapes() {
    let base = tempdir_unique("resolve");
    let root = base.join("share");
    write_file(&root.join("photo.jpg"), "x");
    write_file(&root.join("sub").join("clip.mp4"), "y");
    // A sibling directory the share must never reach.
    write_file(&base.join("secret").join("keys.txt"), "z");

    let canonical = shares::canonical_root(&root.to_string_lossy()).unwrap();

    // Files inside resolve.
    assert!(shares::resolve_within(&canonical, "photo.jpg").is_ok());
    assert!(shares::resolve_within(&canonical, "sub/clip.mp4").is_ok());
    // The empty path is the root itself.
    assert_eq!(
        shares::resolve_within(&canonical, "").unwrap(),
        canonical
    );

    // Traversal to the sibling is refused at the parse stage, before any I/O.
    assert_eq!(
        shares::resolve_within(&canonical, "../secret/keys.txt"),
        Err(PathReject::Traversal)
    );
    // A path that parses cleanly but does not exist is a 404, not a 400.
    assert_eq!(
        shares::resolve_within(&canonical, "nope.jpg"),
        Err(PathReject::NotFound)
    );

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn resolve_within_never_lets_join_replace_the_base() {
    let base = tempdir_unique("join");
    let root = base.join("share");
    write_file(&root.join("a.txt"), "x");
    let canonical = shares::canonical_root(&root.to_string_lossy()).unwrap();

    // Path::join with an absolute component REPLACES the base. split_rel must
    // reject these before they ever reach a join.
    for input in ["C:\\Windows\\win.ini", "/etc/passwd", "\\\\?\\C:\\Windows"] {
        assert_eq!(
            shares::resolve_within(&canonical, input),
            Err(PathReject::Absolute),
            "join-replacement not prevented for {input:?}"
        );
    }

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn resolve_new_within_validates_the_leaf_name() {
    let base = tempdir_unique("newwithin");
    let root = base.join("share");
    fs::create_dir_all(root.join("inbox")).unwrap();
    let canonical = shares::canonical_root(&root.to_string_lossy()).unwrap();

    // A fresh name in an existing directory resolves even though it does not
    // exist yet -- that is the whole point of this function.
    let target = shares::resolve_new_within(&canonical, "inbox", "new.jpg").unwrap();
    assert!(shares::contained_in(&canonical, &target));
    assert!(!target.exists());

    // A traversal in the LEAF (an attacker-controlled multipart filename) is
    // refused.
    assert_eq!(
        shares::resolve_new_within(&canonical, "inbox", ".."),
        Err(PathReject::Traversal)
    );
    assert!(shares::resolve_new_within(&canonical, "inbox", "a/b").is_err());
    assert!(shares::resolve_new_within(&canonical, "inbox", "CON").is_err());
    // A traversal in the DIRECTORY is refused too.
    assert_eq!(
        shares::resolve_new_within(&canonical, "..", "new.jpg"),
        Err(PathReject::Traversal)
    );

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn canonical_root_rejects_missing_and_empty_paths() {
    assert!(shares::canonical_root("").is_err());
    assert!(shares::canonical_root("   ").is_err());
    assert!(shares::canonical_root("/definitely/not/a/real/path/here").is_err());
}

/// On Windows `canonical_root` returns the verbatim `\\?\` form. Both sides of
/// every containment check must carry the same prefix, so this is load-bearing.
#[cfg(windows)]
#[test]
fn canonical_root_returns_verbatim_form_on_windows() {
    let base = tempdir_unique("verbatim");
    let canonical = shares::canonical_root(&base.to_string_lossy()).unwrap();
    assert!(
        canonical.to_string_lossy().starts_with(r"\\?\"),
        "expected verbatim prefix, got {}",
        canonical.display()
    );
    // And display_path strips it again for the UI and Explorer.
    assert!(!shares::display_path(&canonical).starts_with(r"\\?\"));
    let _ = fs::remove_dir_all(&base);
}

// ===========================================================================
// Share resolution and listing
// ===========================================================================

#[test]
fn list_directory_lists_files_and_folders() {
    let base = tempdir_unique("listing");
    let root = base.join("share");
    write_file(&root.join("a.jpg"), "1");
    write_file(&root.join("b.mp4"), "22");
    fs::create_dir_all(root.join("nested")).unwrap();

    let share = share_at(&root, "Test");
    let settings = default_settings();
    let listing = shares::list_directory(&share, "", &settings, None, None).unwrap();

    assert_eq!(listing.dir_count, 1);
    assert_eq!(listing.file_count, 2);
    assert_eq!(listing.total_bytes, 3);
    // Directories always lead, regardless of sort.
    assert!(listing.entries[0].is_dir);
    assert_eq!(listing.entries[0].name, "nested");
    assert!(listing.parent.is_none());

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn list_directory_builds_breadcrumbs_and_parent() {
    let base = tempdir_unique("crumbs");
    let root = base.join("share");
    write_file(&root.join("x").join("y").join("z.txt"), "1");

    let share = share_at(&root, "Test");
    let listing =
        shares::list_directory(&share, "x/y", &default_settings(), None, None).unwrap();

    assert_eq!(listing.path, "x/y");
    assert_eq!(listing.parent.as_deref(), Some("x"));
    assert_eq!(listing.breadcrumbs.len(), 2);
    assert_eq!(listing.breadcrumbs[0].path, "x");
    assert_eq!(listing.breadcrumbs[1].path, "x/y");

    // One level up, the parent is the root (empty string), not None.
    let up = shares::list_directory(&share, "x", &default_settings(), None, None).unwrap();
    assert_eq!(up.parent.as_deref(), Some(""));

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn list_directory_hides_dotfiles_unless_show_hidden() {
    let base = tempdir_unique("hidden");
    let root = base.join("share");
    write_file(&root.join("visible.txt"), "1");
    write_file(&root.join(".hidden"), "1");
    write_file(&root.join("desktop.ini"), "1");

    let share = share_at(&root, "Test");
    let mut settings = default_settings();

    let listing = shares::list_directory(&share, "", &settings, None, None).unwrap();
    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"visible.txt"));
    assert!(!names.contains(&".hidden"));
    assert!(!names.contains(&"desktop.ini"));

    settings.show_hidden = true;
    let listing = shares::list_directory(&share, "", &settings, None, None).unwrap();
    assert_eq!(listing.entries.len(), 3);

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn list_directory_applies_extension_filters() {
    let base = tempdir_unique("extfilter");
    let root = base.join("share");
    write_file(&root.join("keep.jpg"), "1");
    write_file(&root.join("drop.exe"), "1");

    let mut share = share_at(&root, "Test");
    share.cfg.include_ext = vec!["jpg".to_string()];

    let listing =
        shares::list_directory(&share, "", &default_settings(), None, None).unwrap();
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].name, "keep.jpg");

    // The filter must apply to direct fetches too, or a filtered-out file is
    // still downloadable by anyone who guesses its name.
    assert_eq!(
        shares::resolve_file(&share, "drop.exe"),
        Err(PathReject::NotFound)
    );
    assert!(shares::resolve_file(&share, "keep.jpg").is_ok());

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn non_recursive_share_exposes_only_its_top_level() {
    let base = tempdir_unique("nonrecursive");
    let root = base.join("share");
    write_file(&root.join("top.txt"), "1");
    write_file(&root.join("deep").join("inner.txt"), "1");

    let mut share = share_at(&root, "Test");
    share.cfg.recursive = false;

    let listing =
        shares::list_directory(&share, "", &default_settings(), None, None).unwrap();
    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["top.txt"]);

    // And the subfolder is unreachable, not merely unlisted.
    assert_eq!(
        shares::list_directory(&share, "deep", &default_settings(), None, None).err(),
        Some(PathReject::NotFound)
    );
    assert_eq!(
        shares::resolve_file(&share, "deep/inner.txt"),
        Err(PathReject::NotFound)
    );

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn disabled_or_missing_share_serves_nothing() {
    let base = tempdir_unique("disabled");
    let root = base.join("share");
    write_file(&root.join("a.txt"), "1");

    let mut share = share_at(&root, "Test");
    share.cfg.enabled = false;
    assert!(!shares::is_servable(&share));
    assert_eq!(
        shares::list_directory(&share, "", &default_settings(), None, None).err(),
        Some(PathReject::NotFound)
    );
    assert_eq!(shares::resolve_file(&share, "a.txt"), Err(PathReject::NotFound));

    share.cfg.enabled = true;
    share.root_exists = false;
    assert!(!shares::is_servable(&share));

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn single_file_share_exposes_exactly_one_entry() {
    let base = tempdir_unique("singlefile");
    let file = base.join("movie.mp4");
    write_file(&file, "abcd");

    let mut share = share_at(&base, "Movie");
    share.root = shares::canonical_root(&file.to_string_lossy()).unwrap();
    share.cfg.is_file = true;

    let listing =
        shares::list_directory(&share, "", &default_settings(), None, None).unwrap();
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].name, "movie.mp4");
    assert_eq!(listing.entries[0].size, 4);

    // Addressable by name, in either casing; nothing else resolves.
    assert!(shares::resolve_file(&share, "movie.mp4").is_ok());
    assert!(shares::resolve_file(&share, "MOVIE.MP4").is_ok());
    assert_eq!(shares::resolve_file(&share, "other.mp4"), Err(PathReject::NotFound));
    // And it cannot be turned into a folder share.
    assert_eq!(
        shares::list_directory(&share, "sub", &default_settings(), None, None).err(),
        Some(PathReject::NotFound)
    );

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn resolve_file_refuses_directories_and_empty_paths() {
    let base = tempdir_unique("resolvefile");
    let root = base.join("share");
    fs::create_dir_all(root.join("folder")).unwrap();

    let share = share_at(&root, "Test");
    assert_eq!(shares::resolve_file(&share, "folder"), Err(PathReject::NotFound));
    assert_eq!(shares::resolve_file(&share, ""), Err(PathReject::NotFound));
    // And the mirror: resolve_dir refuses files.
    write_file(&root.join("a.txt"), "1");
    assert_eq!(shares::resolve_dir(&share, "a.txt"), Err(PathReject::NotFound));
    assert!(shares::resolve_dir(&share, "folder").is_ok());

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn listing_sorts_by_name_size_and_date() {
    let base = tempdir_unique("sorting");
    let root = base.join("share");
    write_file(&root.join("Banana.txt"), "xxx");
    write_file(&root.join("apple.txt"), "x");

    let share = share_at(&root, "Test");
    let settings = default_settings();

    // Case-insensitive, so it reads the way a file manager shows it rather
    // than putting every capitalised name first.
    let by_name = shares::list_directory(&share, "", &settings, Some("name"), Some(true)).unwrap();
    assert_eq!(by_name.entries[0].name, "apple.txt");

    let by_size = shares::list_directory(&share, "", &settings, Some("size"), Some(false)).unwrap();
    assert_eq!(by_size.entries[0].name, "Banana.txt");

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn index_share_counts_files_dirs_and_bytes() {
    let base = tempdir_unique("index");
    let root = base.join("share");
    write_file(&root.join("a.txt"), "12345");
    write_file(&root.join("sub").join("b.txt"), "678");

    let share = share_at(&root, "Test");
    let (files, dirs, bytes, skipped) = shares::index_share(&share, |_, _| {});
    assert_eq!(files, 2);
    assert_eq!(dirs, 1);
    assert_eq!(bytes, 8);
    assert_eq!(skipped, 0);

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn can_upload_to_requires_every_condition() {
    let base = tempdir_unique("upload_gate");
    let root = base.join("share");
    fs::create_dir_all(&root).unwrap();

    let mut share = share_at(&root, "Inbox");
    let mut settings = default_settings();

    // All four gates default closed.
    assert!(!shares::can_upload_to(&share, &settings));

    settings.uploads_enabled = true;
    assert!(!shares::can_upload_to(&share, &settings), "needs is_inbox");

    share.cfg.is_inbox = true;
    assert!(!shares::can_upload_to(&share, &settings), "needs !read_only");

    share.cfg.read_only = false;
    assert!(shares::can_upload_to(&share, &settings));

    // A single-file share can never be an inbox.
    share.cfg.is_file = true;
    assert!(!shares::can_upload_to(&share, &settings));

    let _ = fs::remove_dir_all(&base);
}

// ===========================================================================
// Auth
// ===========================================================================

#[test]
fn ct_eq_str_matches_only_identical_strings() {
    assert!(auth::ct_eq_str("123456", "123456"));
    assert!(!auth::ct_eq_str("123456", "123457"));
    assert!(!auth::ct_eq_str("123456", "12345"));
    assert!(!auth::ct_eq_str("", "1"));
    assert!(auth::ct_eq_str("", ""));
}

#[test]
fn generated_tokens_are_long_and_unique() {
    let a = auth::random_token();
    let b = auth::random_token();
    assert_eq!(a.len(), 26);
    assert_ne!(a, b);
    // The alphabet omits I, L, O and U so a token read aloud is unambiguous.
    assert!(!a.contains('I') && !a.contains('L') && !a.contains('O') && !a.contains('U'));
}

#[test]
fn generated_pins_are_six_digits() {
    for _ in 0..50 {
        let pin = auth::random_pin();
        assert_eq!(pin.len(), 6);
        assert!(pin.chars().all(|c| c.is_ascii_digit()));
    }
}

#[test]
fn parse_cookie_finds_the_named_value_only() {
    let header = "other=1; lanshare_sid=ABC123; trailing=2";
    assert_eq!(
        auth::parse_cookie(header, "lanshare_sid").as_deref(),
        Some("ABC123")
    );
    assert_eq!(auth::parse_cookie(header, "missing"), None);
    // A cookie whose name merely ends with ours must not match.
    assert_eq!(auth::parse_cookie("xlanshare_sid=NO", "lanshare_sid"), None);
}

#[test]
fn session_cookie_carries_the_hardening_attributes() {
    let cookie = auth::session_cookie("TOKEN", 12);
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Lax"));
    assert!(cookie.contains("Max-Age=43200"));
    // No Secure attribute: plaintext HTTP is forced on a LAN, and setting
    // Secure would stop the cookie being sent at all.
    assert!(!cookie.contains("Secure"));
}

#[test]
fn pin_check_locks_out_after_the_configured_attempts() {
    use std::{collections::HashMap, net::IpAddr, sync::Arc, sync::Mutex};

    let attempts = Arc::new(Mutex::new(HashMap::new()));
    let ip: IpAddr = "192.168.1.50".parse().unwrap();

    // Two wrong tries, counting down.
    for expected_left in [2u32, 1] {
        match auth::check_pin(&attempts, ip, "000000", "123456", 3, 30) {
            auth::PinCheck::Wrong { attempts_left } => assert_eq!(attempts_left, expected_left),
            other => panic!("expected Wrong, got {:?}", matches!(other, auth::PinCheck::Ok)),
        }
    }
    // The third trips the lockout.
    assert!(matches!(
        auth::check_pin(&attempts, ip, "000000", "123456", 3, 30),
        auth::PinCheck::Locked { .. }
    ));
    // And even the CORRECT PIN is refused while locked -- otherwise the
    // lockout would only slow down a wrong guess, not a guessing campaign.
    assert!(matches!(
        auth::check_pin(&attempts, ip, "123456", "123456", 3, 30),
        auth::PinCheck::Locked { .. }
    ));
}

#[test]
fn pin_check_accepts_the_right_pin_and_resets_the_counter() {
    use std::{collections::HashMap, net::IpAddr, sync::Arc, sync::Mutex};

    let attempts = Arc::new(Mutex::new(HashMap::new()));
    let ip: IpAddr = "10.0.0.7".parse().unwrap();

    assert!(matches!(
        auth::check_pin(&attempts, ip, "999999", "123456", 5, 30),
        auth::PinCheck::Wrong { .. }
    ));
    assert!(matches!(
        auth::check_pin(&attempts, ip, "123456", "123456", 5, 30),
        auth::PinCheck::Ok
    ));
    // The counter reset, so a later mistake starts from the full allowance.
    match auth::check_pin(&attempts, ip, "999999", "123456", 5, 30) {
        auth::PinCheck::Wrong { attempts_left } => assert_eq!(attempts_left, 4),
        _ => panic!("expected Wrong"),
    }
}

/// An empty configured PIN must never authenticate. Otherwise a config in
/// which the PIN was cleared by hand would silently be wide open.
#[test]
fn empty_pin_never_authenticates() {
    use std::{collections::HashMap, net::IpAddr, sync::Arc, sync::Mutex};

    let attempts = Arc::new(Mutex::new(HashMap::new()));
    let ip: IpAddr = "10.0.0.8".parse().unwrap();
    assert!(!matches!(
        auth::check_pin(&attempts, ip, "", "", 5, 30),
        auth::PinCheck::Ok
    ));
}

#[test]
fn share_registry_looks_up_by_id_and_token() {
    let base = tempdir_unique("registry");
    let root = base.join("share");
    fs::create_dir_all(&root).unwrap();

    let share = share_at(&root, "Test");
    let registry = ShareRegistry {
        shares: vec![share.clone()],
        ephemeral: Vec::new(),
    };

    assert!(registry.by_id("test").is_some());
    assert!(registry.by_id("nope").is_none());
    assert!(registry.by_token("TESTTOKEN").is_some());
    assert!(registry.by_token("WRONGTOKEN").is_none());
    // An empty token must never match, or a share whose token was cleared by
    // hand would be reachable by anyone sending "/s/".
    assert!(registry.by_token("").is_none());

    // A disabled share is invisible by token, so toggling it off kills the link.
    let mut disabled = share;
    disabled.cfg.enabled = false;
    let registry = ShareRegistry {
        shares: vec![disabled],
        ephemeral: Vec::new(),
    };
    assert!(registry.by_token("TESTTOKEN").is_none());

    let _ = fs::remove_dir_all(&base);
}

// ===========================================================================
// Media classification and thumbnails
// ===========================================================================

#[test]
fn classify_maps_extensions_to_kinds() {
    assert_eq!(media::classify(true, ""), "dir");
    assert_eq!(media::classify(false, "jpg"), "image");
    assert_eq!(media::classify(false, "JPG"), "image");
    assert_eq!(media::classify(false, "mp4"), "video");
    assert_eq!(media::classify(false, "mkv"), "video");
    assert_eq!(media::classify(false, "mp3"), "audio");
    assert_eq!(media::classify(false, "pdf"), "pdf");
    assert_eq!(media::classify(false, "zip"), "archive");
    assert_eq!(media::classify(false, "md"), "text");
    assert_eq!(media::classify(false, "bin"), "other");
    assert_eq!(media::classify(false, ""), "other");
}

/// The point of `playable`: these are media files no browser can decode, and
/// showing a broken player for them is worse than an honest download card.
#[test]
fn unplayable_media_formats_are_marked_unplayable() {
    for ext in ["mkv", "avi", "wmv", "flv", "mpg", "3gp"] {
        assert!(
            !media::is_browser_playable("video", ext),
            "{ext} should not be marked playable"
        );
    }
    for ext in ["mp4", "webm", "m4v", "mov"] {
        assert!(media::is_browser_playable("video", ext), "{ext} should play");
    }
    assert!(!media::is_browser_playable("image", "heic"));
    assert!(media::is_browser_playable("image", "jpg"));
    assert!(!media::is_browser_playable("audio", "wma"));
    assert!(media::is_browser_playable("audio", "mp3"));
    assert!(!media::is_browser_playable("archive", "zip"));
}

#[test]
fn can_thumbnail_excludes_formats_we_cannot_decode() {
    assert!(media::can_thumbnail("jpg"));
    assert!(media::can_thumbnail("PNG"));
    assert!(media::can_thumbnail("webp"));
    // No C libraries for these, and rasterizing untrusted SVG is a different
    // risk surface entirely.
    assert!(!media::can_thumbnail("heic"));
    assert!(!media::can_thumbnail("avif"));
    assert!(!media::can_thumbnail("svg"));
    // Video has no thumbnailer at all -- there is no ffmpeg on the host.
    assert!(!media::can_thumbnail("mp4"));
}

/// iOS refuses to play a video served as application/octet-stream, silently.
#[test]
fn mime_for_returns_playable_types_for_video() {
    assert_eq!(media::mime_for(std::path::Path::new("a.mp4")), "video/mp4");
    assert!(media::mime_for(std::path::Path::new("a.webm")).starts_with("video/"));
    assert!(media::mime_for(std::path::Path::new("a.mp3")).starts_with("audio/"));
    assert!(media::mime_for(std::path::Path::new("a.jpg")).starts_with("image/"));
    assert_eq!(
        media::mime_for(std::path::Path::new("a.mkv")),
        "video/x-matroska"
    );
    assert_eq!(
        media::mime_for(std::path::Path::new("a.unknownext")),
        "application/octet-stream"
    );
}

#[test]
fn thumbnail_cache_key_changes_with_every_input() {
    let path = std::path::Path::new("/media/photo.jpg");
    let base = media::cache_key(path, 1000, 500, 320, 78);

    assert_eq!(base, media::cache_key(path, 1000, 500, 320, 78));
    // A re-saved file must miss the cache.
    assert_ne!(base, media::cache_key(path, 1001, 500, 320, 78));
    assert_ne!(base, media::cache_key(path, 1000, 501, 320, 78));
    // Changing the thumbnail settings must not serve the old size.
    assert_ne!(base, media::cache_key(path, 1000, 500, 480, 78));
    assert_ne!(base, media::cache_key(path, 1000, 500, 320, 90));
    // Different file, same stats.
    assert_ne!(base, media::cache_key(std::path::Path::new("/media/other.jpg"), 1000, 500, 320, 78));
}

#[test]
fn cache_path_shards_by_key_prefix() {
    let dir = std::path::Path::new("/cache");
    let key = "abcdef0123456789";
    let path = media::cache_path(dir, key);
    // A flat directory of 20k files makes every stat slower and Explorer
    // unusable, so keys are sharded two characters deep.
    assert!(path.ends_with(format!("{key}.jpg")));
    assert_eq!(
        path.parent().unwrap().file_name().unwrap().to_string_lossy(),
        "ab"
    );
}

#[test]
fn thumbnail_generates_and_then_reuses_the_cache() {
    let base = tempdir_unique("thumb");
    let cache = base.join("cache");
    fs::create_dir_all(&cache).unwrap();
    let source = base.join("tiny.png");

    // A 4x4 red PNG, encoded through the same library that will read it.
    let img = image::RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0]));
    img.save(&source).unwrap();

    let (bytes, key) = media::thumbnail(&source, &cache, 320, 78).unwrap();
    assert!(!bytes.is_empty());
    // JPEG magic -- proof it re-encoded rather than passing the PNG through.
    assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
    assert!(media::cache_path(&cache, &key).exists());

    let (again, key2) = media::thumbnail(&source, &cache, 320, 78).unwrap();
    assert_eq!(key, key2);
    assert_eq!(bytes, again);

    let (count, total) = media::cache_stats(&cache);
    assert_eq!(count, 1);
    assert!(total > 0);

    let freed = media::clear_cache(&cache).unwrap();
    assert!(freed > 0);
    assert_eq!(media::cache_stats(&cache).0, 0);

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn thumbnail_fails_cleanly_on_a_non_image() {
    let base = tempdir_unique("thumbfail");
    let cache = base.join("cache");
    fs::create_dir_all(&cache).unwrap();
    let source = base.join("not-an-image.jpg");
    write_file(&source, "this is definitely not a JPEG");

    // An Err, not a panic: a share can contain files the host never inspected.
    assert!(media::thumbnail(&source, &cache, 320, 78).is_err());

    let _ = fs::remove_dir_all(&base);
}

// ===========================================================================
// Upload safety
// ===========================================================================

#[test]
fn unique_destination_never_overwrites() {
    let base = tempdir_unique("unique");
    write_file(&base.join("photo.jpg"), "1");

    // The name is taken, so a suffix is added rather than the file replaced.
    let first = utils::unique_destination(&base, "photo.jpg").unwrap();
    assert_eq!(first.file_name().unwrap(), "photo (2).jpg");

    write_file(&first, "2");
    let second = utils::unique_destination(&base, "photo.jpg").unwrap();
    assert_eq!(second.file_name().unwrap(), "photo (3).jpg");

    // A free name is used as-is.
    let fresh = utils::unique_destination(&base, "other.jpg").unwrap();
    assert_eq!(fresh.file_name().unwrap(), "other.jpg");

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn split_name_keeps_dotfiles_whole() {
    assert_eq!(utils::split_name("photo.jpg"), ("photo".into(), ".jpg".into()));
    assert_eq!(
        utils::split_name("archive.tar.gz"),
        ("archive.tar".into(), ".gz".into())
    );
    assert_eq!(utils::split_name("README"), ("README".into(), "".into()));
    // A leading dot marks a hidden file; it is not an extension separator.
    assert_eq!(utils::split_name(".gitignore"), (".gitignore".into(), "".into()));
}

/// A multipart filename is fully attacker-controlled. Taking the last segment
/// and then validating it is what stops `../../../evil.exe`.
#[test]
fn upload_filename_sanitization_strips_directories() {
    let cases = [
        ("../../../evil.exe", "evil.exe"),
        ("..\\..\\evil.exe", "evil.exe"),
        ("C:\\Windows\\System32\\evil.dll", "evil.dll"),
        ("photo.jpg", "photo.jpg"),
    ];
    for (input, expected) in cases {
        let base = input.rsplit(['/', '\\']).next().unwrap();
        assert_eq!(base, expected);
        assert!(shares::check_segment(base).is_ok());
    }
    // And a filename that IS a traversal segment is refused outright.
    assert!(shares::check_segment("..").is_err());
}

// ===========================================================================
// Config
// ===========================================================================

#[test]
fn default_config_is_safe_out_of_the_box() {
    let config = AppConfig::default();
    // Every one of these defaults is a deliberate safety choice.
    assert!(config.pin_enabled, "PIN must be on by default");
    assert!(!config.uploads_enabled, "uploads must be off by default");
    assert!(
        !config.autostart_server,
        "autostart must be off so the firewall prompt lands on a click"
    );
    assert!(config.strict_host_check, "DNS-rebinding guard must be on");
    assert!(!config.show_hidden);
    assert!(config.upload_dedupe_names, "must never overwrite");
    assert_eq!(config.bind_address, "0.0.0.0");
    assert!(config.shares.is_empty());
}

/// A config file missing most of its keys must still load -- that is what all
/// the `#[serde(default)]` attributes buy.
#[test]
fn partial_config_json_loads_with_defaults() {
    let json = r#"{ "language": "en_US", "theme": "nord", "port": 9000 }"#;
    let config: AppConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.theme, "nord");
    assert_eq!(config.port, 9000);
    assert!(config.pin_enabled);
    assert_eq!(config.thumb_max_edge, 320);
    assert!(!config.hidden_names.is_empty());
}

#[test]
fn config_round_trips_through_json() {
    let mut original = AppConfig::default();
    original.port = 9999;
    original.theme = "cyber".to_string();
    original.shares.push(Share {
        id: "abc".to_string(),
        name: "Photos".to_string(),
        path: "/media/photos".to_string(),
        token: "TOK".to_string(),
        enabled: true,
        is_inbox: false,
        read_only: true,
        is_file: false,
        recursive: true,
        include_names: vec![],
        include_ext: vec![],
        exclude_ext: vec![],
        added_ms: 42,
        note: None,
    });

    let text = serde_json::to_string(&original).unwrap();
    let parsed: AppConfig = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed.port, 9999);
    assert_eq!(parsed.shares.len(), 1);
    assert_eq!(parsed.shares[0].name, "Photos");
}

#[test]
fn normalize_backfills_missing_tokens_and_ids() {
    let mut config = AppConfig::default();
    config.shares.push(Share {
        id: String::new(),
        name: "X".to_string(),
        path: "/x".to_string(),
        token: String::new(),
        enabled: true,
        is_inbox: false,
        read_only: true,
        is_file: false,
        recursive: true,
        include_names: vec![],
        include_ext: vec![],
        exclude_ext: vec![],
        added_ms: 0,
        note: None,
    });

    config::normalize(&mut config);
    // An empty token would otherwise be matchable by "/s/".
    assert_eq!(config.shares[0].token.len(), 26);
    assert!(!config.shares[0].id.is_empty());
}

#[test]
fn normalize_enforces_a_single_inbox() {
    let mut config = AppConfig::default();
    for id in ["a", "b", "c"] {
        config.shares.push(Share {
            id: id.to_string(),
            name: id.to_string(),
            path: format!("/{id}"),
            token: auth::random_token(),
            enabled: true,
            is_inbox: true,
            read_only: true,
            is_file: false,
            recursive: true,
            include_names: vec![],
        include_ext: vec![],
            exclude_ext: vec![],
            added_ms: 0,
            note: None,
        });
    }
    config.inbox_share_id = Some("b".to_string());

    config::normalize(&mut config);

    let inboxes: Vec<&str> = config
        .shares
        .iter()
        .filter(|s| s.is_inbox)
        .map(|s| s.id.as_str())
        .collect();
    assert_eq!(inboxes, vec!["b"]);
    assert_eq!(config.inbox_share_id.as_deref(), Some("b"));
}

#[test]
fn normalize_clamps_out_of_range_values() {
    let mut config = AppConfig::default();
    config.thumb_max_edge = 99_999;
    config.thumb_quality = 200;
    config.max_pin_attempts = 0;
    config.port = 0;

    config::normalize(&mut config);

    assert_eq!(config.thumb_max_edge, 1024);
    assert_eq!(config.thumb_quality, 95);
    assert_eq!(config.max_pin_attempts, 1);
    assert_eq!(config.port, crate::models::DEFAULT_PORT);
}

#[test]
fn normalize_cleans_extension_lists() {
    let mut config = AppConfig::default();
    config.upload_allowed_ext = vec![
        ".JPG".to_string(),
        "png ".to_string(),
        "jpg".to_string(),
        "".to_string(),
    ];
    config::normalize(&mut config);
    assert_eq!(config.upload_allowed_ext, vec!["jpg", "png"]);
}

#[test]
fn build_registry_keeps_shares_whose_folders_are_missing() {
    // A disconnected drive must not silently drop the share -- the user would
    // lose its name, token and settings.
    let shares_cfg = vec![Share {
        id: "gone".to_string(),
        name: "External".to_string(),
        path: "/definitely/not/here".to_string(),
        token: auth::random_token(),
        enabled: true,
        is_inbox: false,
        read_only: true,
        is_file: false,
        recursive: true,
        include_names: vec![],
        include_ext: vec![],
        exclude_ext: vec![],
        added_ms: 0,
        note: None,
    }];

    let registry = config::build_registry(&shares_cfg);
    assert_eq!(registry.shares.len(), 1);
    assert!(!registry.shares[0].root_exists);
    assert!(!shares::is_servable(&registry.shares[0]));
}

// ===========================================================================
// Streaming ZIP
//
// Read back with the `zip` crate rather than by asserting our own bytes
// against our own constants -- an independent reader is the only thing that
// proves a hand-rolled archive is actually valid.
// ===========================================================================

fn zip_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut stream = crate::zipstream::ZipStream::new(&mut out);
    for (name, data) in files {
        let mut cursor = std::io::Cursor::new(*data);
        stream
            .write_file(name, data.len() as u64, 1_700_000_000_000, &mut cursor)
            .unwrap();
    }
    stream.finish().unwrap();
    out
}

#[test]
fn zipstream_round_trips_through_an_independent_reader() {
    let bytes = zip_bytes(&[
        ("hello.txt", b"hello world"),
        ("nested/deep/data.bin", &[0u8, 1, 2, 3, 255]),
    ]);

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(archive.len(), 2);

    let mut first = archive.by_name("hello.txt").unwrap();
    let mut contents = String::new();
    std::io::Read::read_to_string(&mut first, &mut contents).unwrap();
    assert_eq!(contents, "hello world");
    drop(first);

    // Forward slashes in entry names are mandated by the format; a backslash is
    // what lets a "zip slip" extractor write outside the target directory.
    let second = archive.by_name("nested/deep/data.bin").unwrap();
    assert_eq!(second.size(), 5);
}

/// The CRC is what a reader validates on extraction, and it is written AFTER
/// the data in a descriptor. A wrong one produces an archive that opens and
/// then fails partway through.
#[test]
fn zipstream_writes_a_correct_crc() {
    let payload = b"the quick brown fox jumps over the lazy dog";
    let bytes = zip_bytes(&[("fox.txt", payload)]);

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut entry = archive.by_index(0).unwrap();
    let mut read_back = Vec::new();
    // The zip crate verifies the CRC as it reads and errors on a mismatch.
    std::io::Read::read_to_end(&mut entry, &mut read_back).unwrap();
    assert_eq!(read_back, payload);
    assert_eq!(entry.crc32(), crc32fast::hash(payload));
}

#[test]
fn zipstream_handles_empty_files_and_unicode_names() {
    let bytes = zip_bytes(&[("empty.txt", b""), ("café/🎬.txt", "movie".as_bytes())]);

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(archive.len(), 2);
    assert_eq!(archive.by_name("empty.txt").unwrap().size(), 0);
    // The UTF-8 flag (bit 11) is what stops a reader decoding this as CP437.
    assert_eq!(archive.by_name("café/🎬.txt").unwrap().size(), 5);
}

#[test]
fn zipstream_produces_a_valid_empty_archive() {
    // A folder that contains only subdirectories still has to yield something
    // an extractor accepts rather than a truncated file.
    let bytes = zip_bytes(&[]);
    let archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(archive.len(), 0);
}

/// Timestamps before the 1980 DOS epoch cannot be represented and must clamp
/// rather than wrap into a nonsense date.
#[test]
fn zipstream_clamps_pre_1980_timestamps() {
    let mut out: Vec<u8> = Vec::new();
    let mut stream = crate::zipstream::ZipStream::new(&mut out);
    let mut cursor = std::io::Cursor::new(b"x".as_slice());
    stream.write_file("old.txt", 1, 0, &mut cursor).unwrap();
    stream.finish().unwrap();

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(out)).unwrap();
    let entry = archive.by_index(0).unwrap();
    let stamp = entry.last_modified().unwrap();
    assert_eq!(stamp.year(), 1980);
}

// ===========================================================================
// Net
// ===========================================================================

#[test]
fn lan_addresses_never_returns_loopback_or_apipa() {
    for addr in net::lan_addresses() {
        let ip: std::net::Ipv4Addr = addr.ip.parse().unwrap();
        assert!(!ip.is_loopback(), "loopback leaked: {}", addr.ip);
        // 169.254.x.x only appears when DHCP failed and is useless to a phone.
        assert!(!ip.is_link_local(), "APIPA leaked: {}", addr.ip);
    }
}

#[test]
fn qr_svg_renders_dark_on_white() {
    let svg = net::qr_svg("http://192.168.1.5:8080", 256).unwrap();
    assert!(svg.starts_with("<?xml") || svg.starts_with("<svg"));
    // Inverted QR codes fail on many Android scanners, so this is load-bearing.
    assert!(svg.contains("#000000"));
    assert!(svg.contains("#ffffff"));
}

#[test]
fn firewall_command_names_the_bound_port() {
    let command = net::firewall_command(8080);
    assert!(command.contains("8080"));
    assert!(!net::firewall_note().is_empty());
}

// ===========================================================================
// HTTP layer
//
// The real router, driven through `tower::ServiceExt::oneshot` -- no socket, no
// Tauri handle. This is what proves the auth gate, the traversal rejection and
// Range streaming actually hold at the HTTP boundary, rather than only in the
// units underneath it.
// ===========================================================================

use std::{
    collections::{HashMap, VecDeque},
    sync::{atomic::AtomicU64, Arc, Mutex, RwLock},
};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt as _;

struct HttpFixture {
    router: axum::Router,
    /// Kept so tests can reach into live state (pending pairs, offers,
    /// transfers) the way the desktop commands do.
    ctx: crate::models::ServerCtx,
    base: PathBuf,
    receive_dir: PathBuf,
    share_id: String,
    share_token: String,
    runtime: tokio::runtime::Runtime,
}

/// Knobs the peer tests need. Everything defaults to the phase-1 behaviour, so
/// `http_fixture("123456")` still means exactly what it used to.
struct FixtureOpts {
    pin: String,
    peers: Vec<Peer>,
    peering_enabled: bool,
    peer_browse_enabled: bool,
}

impl Default for FixtureOpts {
    fn default() -> Self {
        Self {
            pin: "123456".to_string(),
            peers: Vec::new(),
            peering_enabled: true,
            peer_browse_enabled: true,
        }
    }
}

/// Build a router over a temp share. `pin` empty disables the PIN gate.
fn http_fixture(pin: &str) -> HttpFixture {
    fixture_with(FixtureOpts {
        pin: pin.to_string(),
        ..Default::default()
    })
}

/// The single `ServerCtx` construction site. Extracted from `http_fixture` so
/// that adding a field to `ServerCtx` is one edit here rather than one per
/// test module.
fn fixture_with(opts: FixtureOpts) -> HttpFixture {
    let base = tempdir_unique("http");
    let root = base.join("share");
    write_file(&root.join("photo.jpg"), "0123456789");
    write_file(&root.join("sub").join("clip.txt"), "nested");
    // A sibling the share must never be able to reach.
    write_file(&base.join("secret.txt"), "TOP SECRET");

    let receive_dir = base.join("received");
    fs::create_dir_all(&receive_dir).unwrap();
    let receive_dir = shares::canonical_root(&receive_dir.to_string_lossy()).unwrap();

    let mut share = share_at(&root, "Photos");
    share.cfg.id = "shr1".to_string();
    share.cfg.token = "TOKENTOKENTOKENTOKENTOKEN1".to_string();

    let mut config = AppConfig::default();
    config.pin_enabled = !opts.pin.is_empty();
    config.pin = opts.pin.clone();
    config.peering_enabled = opts.peering_enabled;
    config.peer_browse_enabled = opts.peer_browse_enabled;
    config.device_name = "TestHost".to_string();
    let settings = ServerSettings::from_config(&config);

    let ctx = crate::models::ServerCtx {
        settings: Arc::new(RwLock::new(settings)),
        shares: Arc::new(RwLock::new(ShareRegistry {
            shares: vec![share.clone()],
            ephemeral: Vec::new(),
        })),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        pin_attempts: Arc::new(Mutex::new(HashMap::new())),
        activity: Arc::new(Mutex::new(VecDeque::new())),
        next_activity_id: Arc::new(AtomicU64::new(0)),
        bytes_served: Arc::new(AtomicU64::new(0)),
        requests_served: Arc::new(AtomicU64::new(0)),
        thumb_permits: Arc::new(tokio::sync::Semaphore::new(2)),
        thumb_dir: base.join("thumbs"),
        peers: Arc::new(RwLock::new(crate::models::PeerRegistry { peers: opts.peers })),
        discovered: Arc::new(Mutex::new(HashMap::new())),
        pending_pairs: Arc::new(Mutex::new(HashMap::new())),
        pair_attempts: Arc::new(Mutex::new(HashMap::new())),
        offers: Arc::new(Mutex::new(HashMap::new())),
        transfers: Arc::new(Mutex::new(VecDeque::new())),
        next_transfer_id: Arc::new(AtomicU64::new(0)),
        transfer_cancels: Arc::new(Mutex::new(HashMap::new())),
        device_id: Arc::new("hostdevice1".to_string()),
        receive_dir: receive_dir.clone(),
        discovery_self_seen_ms: Arc::new(AtomicU64::new(0)),
        discovery_started_ms: Arc::new(AtomicU64::new(0)),
    };

    HttpFixture {
        router: crate::server::build_router(ctx.clone()),
        ctx,
        base,
        receive_dir,
        share_id: share.cfg.id,
        share_token: share.cfg.token,
        // A current-thread runtime built locally, so tokio's `macros` feature
        // stays out of the dependency list.
        runtime: tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
    }
}

/// A paired device, from the receiving side's point of view.
fn test_peer(id: &str, in_token: &str) -> Peer {
    Peer {
        device_id: id.to_string(),
        name: format!("Peer {id}"),
        platform: "windows".to_string(),
        in_token: in_token.to_string(),
        out_token: "OUTTOKENOUTTOKENOUTTOKEN01".to_string(),
        added_ms: 1,
        last_seen_ms: 1,
        last_address: None,
        auto_accept: false,
        blocked: false,
        note: None,
    }
}

impl HttpFixture {
    fn send(&self, request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let router = self.router.clone();
        self.runtime.block_on(async move {
            let response = router.oneshot(request).await.unwrap();
            let status = response.status();
            let headers = response.headers().clone();
            let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
                .await
                .unwrap();
            (status, headers, bytes.to_vec())
        })
    }

    fn get(&self, uri: &str, cookie: Option<&str>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        self.send(self.build(uri, cookie, None).body(Body::empty()).unwrap())
    }

    /// The peer path: a bearer token instead of a session cookie.
    fn get_as_peer(
        &self,
        uri: &str,
        token: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let request = self
            .build(uri, None, None)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        self.send(request)
    }

    fn post_as_peer(
        &self,
        uri: &str,
        token: &str,
        body: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let request = self
            .build(uri, None, None)
            .method("POST")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        self.send(request)
    }

    /// POST JSON from a chosen source IP, so the pairing IP-binding rules are
    /// testable.
    fn post_from(
        &self,
        ip: [u8; 4],
        uri: &str,
        body: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let request = Request::builder()
            .uri(uri)
            .method("POST")
            .header(header::HOST, "192.168.1.10:8080")
            .header(header::CONTENT_TYPE, "application/json")
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                ip, 50000,
            ))))
            .body(Body::from(body.to_string()))
            .unwrap();
        self.send(request)
    }

    fn json(&self, bytes: &[u8]) -> serde_json::Value {
        serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null)
    }

    fn build(
        &self,
        uri: &str,
        cookie: Option<&str>,
        range: Option<&str>,
    ) -> axum::http::request::Builder {
        let mut builder = Request::builder()
            .uri(uri)
            // The host guard only accepts IP literals and localhost.
            .header(header::HOST, "192.168.1.10:8080")
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [192, 168, 1, 77],
                50000,
            ))));
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        if let Some(range) = range {
            builder = builder.header(header::RANGE, range);
        }
        builder
    }

    /// Exchange the PIN for a session cookie.
    fn login(&self, pin: &str) -> String {
        let request = self
            .build("/api/auth", None, None)
            .method("POST")
            .header(header::CONTENT_TYPE, "application/json")
            .header("X-LanShare", "1")
            .body(Body::from(format!(r#"{{"pin":"{pin}"}}"#)))
            .unwrap();
        let (status, headers, _) = self.send(request);
        assert_eq!(status, StatusCode::OK, "login should succeed");
        let cookie = headers
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        cookie.split(';').next().unwrap().to_string()
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[test]
fn http_serves_the_shell_and_assets_without_auth() {
    let fx = http_fixture("123456");

    // The PIN pad has to render before anyone can authenticate, so the shell
    // and its assets are deliberately public.
    let (status, headers, body) = fx.get("/", None);
    assert_eq!(status, StatusCode::OK);
    assert!(headers.contains_key(header::CONTENT_SECURITY_POLICY));
    assert!(String::from_utf8_lossy(&body).contains("<title>LAN Share</title>"));

    let (status, headers, _) = fx.get("/assets/app.js", None);
    assert_eq!(status, StatusCode::OK);
    assert!(headers.contains_key(header::ETAG));

    let (status, _, body) = fx.get("/api/ping", None);
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["pinRequired"], true);
    // Liveness only -- no share names, no paths, no version of the host OS.
    assert!(json.get("shares").is_none());
}

#[test]
fn http_refuses_every_data_route_without_a_session() {
    let fx = http_fixture("123456");
    for uri in [
        "/api/shares",
        "/api/list?share=shr1&path=",
        "/files/shr1/photo.jpg",
        "/download/shr1/photo.jpg",
    ] {
        let (status, _, _) = fx.get(uri, None);
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} should require auth");
    }
}

#[test]
fn http_rejects_a_wrong_pin_and_accepts_the_right_one() {
    let fx = http_fixture("123456");

    let request = fx
        .build("/api/auth", None, None)
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .header("X-LanShare", "1")
        .body(Body::from(r#"{"pin":"000000"}"#))
        .unwrap();
    let (status, _, body) = fx.send(request);
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "invalid_pin");
    // The client renders a countdown from this.
    assert!(json["attemptsLeft"].is_number());

    let cookie = fx.login("123456");
    assert!(cookie.starts_with("lanshare_sid="));

    let (status, _, body) = fx.get("/api/shares", Some(&cookie));
    assert_eq!(status, StatusCode::OK);
    let shares: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(shares[0]["id"], "shr1");
}

#[test]
fn http_mints_a_session_when_the_pin_is_disabled() {
    let fx = http_fixture("");
    let (status, headers, body) = fx.get("/api/session", None);
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["authenticated"], true);
    assert_eq!(json["pinRequired"], false);
    // The cookie must ride along, or the client would re-mint on every request.
    assert!(headers.contains_key(header::SET_COOKIE));

    // And the data routes are open without any cookie at all.
    let (status, _, _) = fx.get("/api/list?share=shr1&path=", None);
    assert_eq!(status, StatusCode::OK);
}

#[test]
fn http_share_link_scopes_the_session_to_one_share() {
    let fx = http_fixture("123456");

    // 303 See Other (what axum's `Redirect::to` emits) rather than 302: it
    // states unambiguously that the follow-up is a GET.
    let (status, headers, _) = fx.get(&format!("/s/{}", fx.share_token), None);
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get(header::LOCATION).unwrap(),
        &format!("/?s={}", fx.share_id)
    );

    let cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // The scoped session reaches its own share...
    let (status, _, _) = fx.get("/api/list?share=shr1&path=", Some(&cookie));
    assert_eq!(status, StatusCode::OK);

    // ...and cannot even confirm another share id exists: 404, not 403.
    let (status, _, _) = fx.get("/api/list?share=other&path=", Some(&cookie));
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A bad token is a 404 too, so the token space cannot be probed.
    let (status, _, _) = fx.get("/s/NOTAREALTOKENNOTAREALTOKEN", None);
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The whole point of the containment layer, exercised through the real URL
/// parsing rather than by calling `resolve_within` directly.
#[test]
fn http_refuses_path_traversal_out_of_a_share() {
    let fx = http_fixture("123456");
    let cookie = fx.login("123456");

    for uri in [
        // axum percent-decodes the captured segments exactly once, so each of
        // these reaches the handler as a real "../" and must be refused there.
        "/files/shr1/..%2F..%2Fsecret.txt",
        "/files/shr1/..%2Fsecret.txt",
        "/files/shr1/%2E%2E%2Fsecret.txt",
        "/download/shr1/..%2F..%2Fsecret.txt",
    ] {
        let (status, _, _) = fx.get(uri, Some(&cookie));
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND,
            "{uri} returned {status}, expected a rejection"
        );
    }

    // And the listing endpoint, where the path arrives as a query value.
    let (status, _, _) = fx.get("/api/list?share=shr1&path=..%2F..", Some(&cookie));
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The sibling file is genuinely there -- the rejection is the containment
    // check, not a missing file.
    assert!(fx.base.join("secret.txt").exists());
}

#[test]
fn http_serves_a_file_and_answers_range_requests() {
    let fx = http_fixture("123456");
    let cookie = fx.login("123456");

    // Full body.
    let (status, headers, body) = fx.get("/files/shr1/photo.jpg", Some(&cookie));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"0123456789");
    // iOS refuses to play media served as application/octet-stream.
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "image/jpeg");
    // Without this header a browser will not even attempt to seek.
    assert_eq!(headers.get(header::ACCEPT_RANGES).unwrap(), "bytes");
    assert_eq!(headers.get(header::CONTENT_DISPOSITION).unwrap(), "inline");

    // Mid-file range -- this is what scrubbing a video timeline issues.
    let request = fx
        .build("/files/shr1/photo.jpg", Some(&cookie), Some("bytes=2-5"))
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = fx.send(request);
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, b"2345");
    assert_eq!(
        headers.get(header::CONTENT_RANGE).unwrap(),
        "bytes 2-5/10"
    );

    // Open-ended range -- what a player sends to resume.
    let request = fx
        .build("/files/shr1/photo.jpg", Some(&cookie), Some("bytes=7-"))
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = fx.send(request);
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, b"789");

    // Unsatisfiable.
    let request = fx
        .build("/files/shr1/photo.jpg", Some(&cookie), Some("bytes=999-"))
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = fx.send(request);
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
}

#[test]
fn http_download_route_sets_an_attachment_disposition() {
    let fx = http_fixture("123456");
    let cookie = fx.login("123456");

    let (status, headers, body) = fx.get("/download/shr1/photo.jpg", Some(&cookie));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"0123456789");
    let disposition = headers
        .get(header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(disposition.starts_with("attachment"));
    // RFC 5987 form carries the real, possibly non-ASCII, name.
    assert!(disposition.contains("filename*=UTF-8''photo.jpg"));
}

#[test]
fn http_lists_a_directory_with_derivable_entry_data() {
    let fx = http_fixture("123456");
    let cookie = fx.login("123456");

    let (status, _, body) = fx.get("/api/list?share=shr1&path=", Some(&cookie));
    assert_eq!(status, StatusCode::OK);
    let listing: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(listing["share_name"], "Photos");
    assert_eq!(listing["file_count"], 1);
    assert_eq!(listing["dir_count"], 1);

    let entries = listing["entries"].as_array().unwrap();
    // Directories lead regardless of sort.
    assert_eq!(entries[0]["name"], "sub");
    assert_eq!(entries[0]["is_dir"], true);
    assert_eq!(entries[1]["name"], "photo.jpg");
    assert_eq!(entries[1]["kind"], "image");
    assert_eq!(entries[1]["playable"], true);

    // Descend one level.
    let (status, _, body) = fx.get("/api/list?share=shr1&path=sub", Some(&cookie));
    assert_eq!(status, StatusCode::OK);
    let listing: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(listing["parent"], "");
    assert_eq!(listing["entries"][0]["name"], "clip.txt");
}

/// DNS rebinding: a page on an attacker's domain re-points that domain at the
/// victim's LAN IP, then reads this server's responses as same-origin.
#[test]
fn http_host_guard_rejects_hostnames() {
    let fx = http_fixture("123456");

    let request = Request::builder()
        .uri("/api/ping")
        .header(header::HOST, "evil.example.com")
        .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [192, 168, 1, 77],
            50000,
        ))))
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = fx.send(request);
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);

    // localhost and bare IPs are the legitimate ways to reach a LAN server.
    for host in ["localhost:8080", "127.0.0.1:8080", "192.168.1.10:8080"] {
        let request = Request::builder()
            .uri("/api/ping")
            .header(header::HOST, host)
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                50000,
            ))))
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = fx.send(request);
        assert_eq!(status, StatusCode::OK, "{host} should be accepted");
    }
}

#[test]
fn http_upload_is_refused_when_uploads_are_off() {
    let fx = http_fixture("123456");
    let cookie = fx.login("123456");

    let request = fx
        .build("/api/upload?share=shr1&path=", Some(&cookie), None)
        .method("POST")
        .header("X-LanShare", "1")
        .header(
            header::CONTENT_TYPE,
            "multipart/form-data; boundary=XBOUNDARY",
        )
        .body(Body::from(
            "--XBOUNDARY\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nhi\r\n--XBOUNDARY--\r\n",
        ))
        .unwrap();
    let (status, _, _) = fx.send(request);
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// SameSite=Lax already blocks cross-site POSTs; the header check is the second
/// lock, and it must actually be enforced.
#[test]
fn http_upload_requires_the_csrf_header() {
    let fx = http_fixture("123456");
    let cookie = fx.login("123456");

    let request = fx
        .build("/api/upload?share=shr1&path=", Some(&cookie), None)
        .method("POST")
        .header(
            header::CONTENT_TYPE,
            "multipart/form-data; boundary=XBOUNDARY",
        )
        .body(Body::from("--XBOUNDARY--\r\n"))
        .unwrap();
    let (status, _, _) = fx.send(request);
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// `{*path}` does not match an empty segment, so the share-root form needs its
/// own route. Without it the request fell through to the SPA fallback and the
/// client received the HTML shell saved under a `.zip` filename -- a failure
/// that looks exactly like a successful download.
#[test]
fn http_zips_a_share_root_as_well_as_a_subfolder() {
    let fx = http_fixture("123456");
    let cookie = fx.login("123456");

    for uri in ["/zip/shr1", "/zip/shr1/", "/zip/shr1/sub"] {
        let (status, headers, body) = fx.get(uri, Some(&cookie));
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "application/zip",
            "{uri} must not return the HTML shell"
        );
        // "PK\x03\x04" -- a real local file header, not a doctype.
        assert_eq!(&body[..4], b"PK\x03\x04", "{uri} is not a zip archive");
    }
}

/// A malformed data-route URL must 404 rather than quietly hand back the HTML
/// shell under the filename the client asked for.
#[test]
fn http_fallback_never_serves_html_under_a_data_prefix() {
    let fx = http_fixture("123456");
    for uri in [
        "/files/",
        "/download/",
        "/zip/",
        "/assets/nope.js",
        "/api/nope",
    ] {
        let (status, headers, _) = fx.get(uri, None);
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap_or(""))
            .unwrap_or("");
        assert!(!content_type.contains("text/html"), "{uri} returned HTML");
    }
}

#[test]
fn http_unknown_api_route_is_404_and_everything_else_serves_the_shell() {
    let fx = http_fixture("123456");

    let (status, _, _) = fx.get("/api/does-not-exist", None);
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A deep link must survive a hard reload, so non-API paths fall back to
    // the shell rather than 404ing.
    let (status, _, body) = fx.get("/some/deep/link", None);
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("<title>LAN Share</title>"));
}

// ===========================================================================
// Utils
// ===========================================================================

#[test]
fn format_bytes_matches_the_reference_app() {
    assert_eq!(utils::format_bytes(0), "0.0 B");
    assert_eq!(utils::format_bytes(512), "512.0 B");
    assert_eq!(utils::format_bytes(1024), "1.0 KB");
    assert_eq!(utils::format_bytes(1536), "1.5 KB");
    assert_eq!(utils::format_bytes(1024 * 1024), "1.0 MB");
    assert_eq!(utils::format_bytes(1024 * 1024 * 1024), "1.0 GB");
    // Terabyte-scale values stay in GB rather than overflowing the unit list.
    assert_eq!(utils::format_bytes(5 * 1024 * 1024 * 1024 * 1024), "5120.0 GB");
}

#[test]
fn ext_of_lowercases_and_drops_the_dot() {
    assert_eq!(utils::ext_of("photo.JPG"), "jpg");
    assert_eq!(utils::ext_of("archive.tar.gz"), "gz");
    assert_eq!(utils::ext_of("README"), "");
    assert_eq!(utils::ext_of(".gitignore"), "");
}

/// RFC 8187 `attr-char` covers dots and dashes, so a plain filename must come
/// through the `filename*` parameter untouched.
#[test]
fn rfc8187_encode_leaves_attr_chars_alone() {
    assert_eq!(utils::rfc8187_encode("photo.jpg"), "photo.jpg");
    assert_eq!(utils::rfc8187_encode("my-file_v2.tar.gz"), "my-file_v2.tar.gz");
    assert_eq!(utils::rfc8187_encode("a b"), "a%20b");
    assert_eq!(utils::rfc8187_encode("a\"b"), "a%22b");
    assert_eq!(utils::rfc8187_encode("a;b"), "a%3Bb");
    // Non-ASCII becomes UTF-8 percent-escapes, which is the entire reason
    // `filename*` exists alongside the ASCII-only `filename`.
    assert_eq!(utils::rfc8187_encode("café.jpg"), "caf%C3%A9.jpg");
}

#[test]
fn url_encode_escapes_every_url_structural_character() {
    // A filename must never be able to restructure the URL it appears in.
    assert_eq!(utils::url_encode("a b"), "a%20b");
    assert_eq!(utils::url_encode("a/b"), "a%2Fb");
    assert_eq!(utils::url_encode("a&b=c"), "a%26b%3Dc");
    assert_eq!(utils::url_encode("a#b?c"), "a%23b%3Fc");
}

#[test]
fn header_safe_ascii_strips_quote_and_control_characters() {
    // Anything that could terminate the quoted-string or inject a header.
    assert_eq!(header_free(r#"na"me"#), "na_me");
    assert_eq!(header_free("na\nme"), "na_me");
    assert_eq!(header_free("na\\me"), "na_me");
    assert_eq!(header_free("normal name.jpg"), "normal name.jpg");
    // Non-ASCII is replaced; the real name still rides in filename* as UTF-8.
    assert_eq!(header_free("café.jpg"), "caf_.jpg");
}

fn header_free(value: &str) -> String {
    utils::header_safe_ascii(value)
}

#[test]
fn sha256_hex_is_stable_and_full_length() {
    let a = utils::sha256_hex("hello");
    assert_eq!(a.len(), 64);
    assert_eq!(a, utils::sha256_hex("hello"));
    assert_ne!(a, utils::sha256_hex("hellp"));
    // Known vector, so a hash-library swap cannot silently invalidate every
    // on-disk cache entry without this failing first.
    assert_eq!(
        a,
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

// ===========================================================================
// Pairing: the code derivation
//
// Pure functions, no HTTP. This is the security core of the whole feature --
// if the two devices can be made to show matching digits without both users
// consenting, everything downstream is theatre.
// ===========================================================================

use crate::peers::{commit_of, pair_code};

#[test]
fn pair_code_is_identical_on_both_sides() {
    let nonce_a = "AAAAAAAAAAAAAAAAAAAAAAAAAA";
    let nonce_b = "BBBBBBBBBBBBBBBBBBBBBBBBBB";
    let commit = commit_of(nonce_a);

    // Initiator computes it from what it holds; responder from what it stored.
    let from_a = pair_code(&commit, nonce_a, nonce_b, "deva", "devb");
    let from_b = pair_code(&commit, nonce_a, nonce_b, "deva", "devb");

    assert_eq!(from_a, from_b);
    assert_eq!(from_a.len(), 6);
    assert!(from_a.chars().all(|c| c.is_ascii_digit()));
}

#[test]
fn pair_code_changes_when_any_input_changes() {
    let base = pair_code("commit", "na", "nb", "ida", "idb");
    assert_ne!(base, pair_code("commitX", "na", "nb", "ida", "idb"));
    assert_ne!(base, pair_code("commit", "naX", "nb", "ida", "idb"));
    assert_ne!(base, pair_code("commit", "na", "nbX", "ida", "idb"));
    assert_ne!(base, pair_code("commit", "na", "nb", "idaX", "idb"));
    assert_ne!(base, pair_code("commit", "na", "nb", "ida", "idbX"));
}

/// Without length prefixing, `("ab","c")` and `("a","bc")` hash identically.
/// A device chooses its own id, so that is a collision an attacker can steer
/// into deliberately -- not a theoretical one.
#[test]
fn pair_code_is_length_prefixed() {
    assert_ne!(
        pair_code("c", "n", "n", "ab", "c"),
        pair_code("c", "n", "n", "a", "bc")
    );
    assert_ne!(
        pair_code("c", "ab", "c", "i", "j"),
        pair_code("c", "a", "bc", "i", "j")
    );
}

/// The commitment is what removes the attacker's grinding window. If it did not
/// bind, an on-path device could pick its nonce after seeing the other side's
/// and force both screens to agree.
#[test]
fn commit_binds_the_nonce() {
    let nonce = "REALNONCEREALNONCEREALNON1";
    let commit = commit_of(nonce);

    assert_eq!(commit, commit_of(nonce));
    assert_ne!(commit, commit_of("OTHERNONCEOTHERNONCEOTHER1"));
    // 32 bytes as hex.
    assert_eq!(commit.len(), 64);
    assert!(commit.chars().all(|c| c.is_ascii_hexdigit()));
}

/// A truncation bug that folded too few bytes would pin the leading digits.
#[test]
fn pair_code_covers_the_whole_digit_range() {
    let mut leading = std::collections::HashSet::new();
    let mut seen = std::collections::HashMap::new();
    for i in 0..3000 {
        let code = pair_code("c", &format!("n{i}"), "nb", "ida", "idb");
        leading.insert(code.chars().next().unwrap());
        *seen.entry(code).or_insert(0) += 1;
    }
    assert_eq!(leading.len(), 10, "leading digit is not uniform");
    // With 3000 draws from a million values, any repeat at all is unlikely and
    // three of the same value means the output space collapsed.
    assert!(seen.values().all(|n| *n <= 3));
}

// ===========================================================================
// Pairing over HTTP
// ===========================================================================

fn pair_request_body(commit: &str) -> String {
    format!(
        r#"{{"device_id":"remotedev1","name":"Remote","platform":"linux","port":8080,
             "commit":"{commit}","out_token":"REMOTEOUTTOKENREMOTEOUT01"}}"#
    )
}

/// The single most important property of the request step: the reply must not
/// contain anything derived from the initiator's nonce, because the initiator
/// has not revealed it yet.
#[test]
fn pair_request_replies_without_revealing_a_code() {
    let fx = http_fixture("123456");
    let commit = commit_of("MYNONCEMYNONCEMYNONCEMYNO1");

    let (status, _, body) = fx.post_from([192, 168, 1, 50], "/api/peer/pair/request", &pair_request_body(&commit));
    assert_eq!(status, StatusCode::OK);
    let json = fx.json(&body);
    assert!(json["pair_id"].is_string());
    assert!(json["nonce_b"].is_string());
    assert!(json["code"].is_null(), "the code must not be computable yet");

    // And nothing is shown to the user until the reveal arrives.
    assert!(
        crate::peers::list_prompts_ctx(&fx.ctx).is_empty(),
        "a prompt appeared before the nonce was revealed"
    );
}

#[test]
fn pair_reveal_then_poll_yields_the_token_exactly_once() {
    let fx = http_fixture("123456");
    let nonce_a = "MYNONCEMYNONCEMYNONCEMYNO1";
    let commit = commit_of(nonce_a);
    let ip = [192, 168, 1, 50];

    let (_, _, body) = fx.post_from(ip, "/api/peer/pair/request", &pair_request_body(&commit));
    let pair_id = fx.json(&body)["pair_id"].as_str().unwrap().to_string();
    let nonce_b = fx.json(&body)["nonce_b"].as_str().unwrap().to_string();

    let (status, _, body) = fx.post_from(
        ip,
        "/api/peer/pair/reveal",
        &format!(r#"{{"pair_id":"{pair_id}","nonce":"{nonce_a}"}}"#),
    );
    assert_eq!(status, StatusCode::OK);
    let echoed = fx.json(&body)["code"].as_str().unwrap().to_string();
    assert_eq!(
        echoed,
        pair_code(&commit, nonce_a, &nonce_b, "remotedev1", "hostdevice1")
    );

    // NOW the user sees a prompt, and it carries the same digits.
    let prompts = crate::peers::list_prompts_ctx(&fx.ctx);
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].code, echoed);

    // Still pending until someone answers.
    let (_, _, body) = fx.post_from(ip, "/api/peer/pair/poll", &format!(r#"{{"pair_id":"{pair_id}"}}"#));
    assert_eq!(fx.json(&body)["state"], "pending");

    crate::peers::accept_prompt_ctx(&fx.ctx, &pair_id).unwrap();

    let (status, _, body) = fx.post_from(ip, "/api/peer/pair/poll", &format!(r#"{{"pair_id":"{pair_id}"}}"#));
    assert_eq!(status, StatusCode::OK);
    let json = fx.json(&body);
    assert_eq!(json["state"], "accepted");
    assert!(!json["token"].as_str().unwrap().is_empty());

    // Single-use: the token is handed over exactly once.
    let (status, _, _) = fx.post_from(ip, "/api/peer/pair/poll", &format!(r#"{{"pair_id":"{pair_id}"}}"#));
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A poll from elsewhere must be refused WITHOUT consuming the entry, or a
/// guesser that happened to hit a live pair_id could destroy a legitimate
/// pairing it cannot itself complete.
#[test]
fn pair_poll_from_another_ip_is_refused_without_consuming() {
    let fx = http_fixture("123456");
    let nonce_a = "MYNONCEMYNONCEMYNONCEMYNO1";
    let commit = commit_of(nonce_a);
    let ip = [192, 168, 1, 50];

    let (_, _, body) = fx.post_from(ip, "/api/peer/pair/request", &pair_request_body(&commit));
    let pair_id = fx.json(&body)["pair_id"].as_str().unwrap().to_string();
    fx.post_from(
        ip,
        "/api/peer/pair/reveal",
        &format!(r#"{{"pair_id":"{pair_id}","nonce":"{nonce_a}"}}"#),
    );
    crate::peers::accept_prompt_ctx(&fx.ctx, &pair_id).unwrap();

    let (status, _, _) = fx.post_from(
        [10, 0, 0, 9],
        "/api/peer/pair/poll",
        &format!(r#"{{"pair_id":"{pair_id}"}}"#),
    );
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The real initiator can still collect it.
    let (status, _, body) = fx.post_from(ip, "/api/peer/pair/poll", &format!(r#"{{"pair_id":"{pair_id}"}}"#));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fx.json(&body)["state"], "accepted");
}

#[test]
fn pair_reveal_with_a_wrong_nonce_destroys_the_exchange() {
    let fx = http_fixture("123456");
    let commit = commit_of("REALNONCEREALNONCEREALNON1");
    let ip = [192, 168, 1, 50];

    let (_, _, body) = fx.post_from(ip, "/api/peer/pair/request", &pair_request_body(&commit));
    let pair_id = fx.json(&body)["pair_id"].as_str().unwrap().to_string();

    let (status, _, _) = fx.post_from(
        ip,
        "/api/peer/pair/reveal",
        &format!(r#"{{"pair_id":"{pair_id}","nonce":"WRONGNONCEWRONGNONCEWRONG1"}}"#),
    );
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Dropped entirely, so a second guess cannot be made against it.
    let (status, _, _) = fx.post_from(ip, "/api/peer/pair/poll", &format!(r#"{{"pair_id":"{pair_id}"}}"#));
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test]
fn pair_reveal_is_single_use() {
    let fx = http_fixture("123456");
    let nonce_a = "MYNONCEMYNONCEMYNONCEMYNO1";
    let commit = commit_of(nonce_a);
    let ip = [192, 168, 1, 50];

    let (_, _, body) = fx.post_from(ip, "/api/peer/pair/request", &pair_request_body(&commit));
    let pair_id = fx.json(&body)["pair_id"].as_str().unwrap().to_string();
    let reveal = format!(r#"{{"pair_id":"{pair_id}","nonce":"{nonce_a}"}}"#);

    assert_eq!(fx.post_from(ip, "/api/peer/pair/reveal", &reveal).0, StatusCode::OK);
    // Second reveal: the state has moved on, so it is refused.
    assert_eq!(
        fx.post_from(ip, "/api/peer/pair/reveal", &reveal).0,
        StatusCode::NOT_FOUND
    );
}

#[test]
fn only_one_live_pair_prompt_per_ip() {
    let fx = http_fixture("123456");
    let commit = commit_of("N1N1N1N1N1N1N1N1N1N1N1N1N1");
    let ip = [192, 168, 1, 50];

    assert_eq!(fx.post_from(ip, "/api/peer/pair/request", &pair_request_body(&commit)).0, StatusCode::OK);
    let (status, _, body) = fx.post_from(ip, "/api/peer/pair/request", &pair_request_body(&commit));
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(fx.json(&body)["error"], "pair_in_progress");
    assert_eq!(fx.ctx.pending_pairs.lock().unwrap().len(), 1);
}

/// Failures are counted, attempts are not. Counting every request would lock a
/// user out of their own app for pairing five devices in a row on a first run.
#[test]
fn repeated_valid_pair_requests_are_not_rate_limited() {
    let fx = http_fixture("123456");
    let commit = commit_of("N1N1N1N1N1N1N1N1N1N1N1N1N1");

    for round in 0..(crate::models::PAIR_MAX_ATTEMPTS + 3) {
        let (status, _, _) = fx.post_from(
            [192, 168, 1, 50],
            "/api/peer/pair/request",
            &pair_request_body(&commit),
        );
        assert_eq!(status, StatusCode::OK, "round {round} was refused");
        // Clear the prompt so the one-per-IP rule is not what we are measuring.
        fx.ctx.pending_pairs.lock().unwrap().clear();
    }
}

#[test]
fn repeated_malformed_pair_requests_are_rate_limited() {
    let fx = http_fixture("123456");
    let bad = r#"{"device_id":"","commit":"nope","out_token":""}"#;

    let mut last = StatusCode::OK;
    for _ in 0..(crate::models::PAIR_MAX_ATTEMPTS + 1) {
        last = fx.post_from([192, 168, 1, 50], "/api/peer/pair/request", bad).0;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);

    // Even a well-formed request is refused while locked out.
    let commit = commit_of("N1N1N1N1N1N1N1N1N1N1N1N1N1");
    let (status, _, _) = fx.post_from(
        [192, 168, 1, 50],
        "/api/peer/pair/request",
        &pair_request_body(&commit),
    );
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // A different device is unaffected.
    let (status, _, _) = fx.post_from(
        [192, 168, 1, 77],
        "/api/peer/pair/request",
        &pair_request_body(&commit),
    );
    assert_eq!(status, StatusCode::OK);
}

/// A run of bad requests followed by a good one must not leave the address one
/// mistake away from a lockout.
#[test]
fn a_successful_pair_request_clears_the_failure_count() {
    let fx = http_fixture("123456");
    let bad = r#"{"device_id":"","commit":"nope","out_token":""}"#;
    let commit = commit_of("N1N1N1N1N1N1N1N1N1N1N1N1N1");

    for _ in 0..(crate::models::PAIR_MAX_ATTEMPTS - 1) {
        fx.post_from([192, 168, 1, 50], "/api/peer/pair/request", bad);
    }
    let (status, _, _) = fx.post_from(
        [192, 168, 1, 50],
        "/api/peer/pair/request",
        &pair_request_body(&commit),
    );
    assert_eq!(status, StatusCode::OK);
    fx.ctx.pending_pairs.lock().unwrap().clear();

    // The counter reset, so the next mistake starts from the full allowance.
    let (status, _, _) = fx.post_from([192, 168, 1, 50], "/api/peer/pair/request", bad);
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[test]
fn declining_is_reported_to_the_initiator() {
    let fx = http_fixture("123456");
    let nonce_a = "MYNONCEMYNONCEMYNONCEMYNO1";
    let commit = commit_of(nonce_a);
    let ip = [192, 168, 1, 50];

    let (_, _, body) = fx.post_from(ip, "/api/peer/pair/request", &pair_request_body(&commit));
    let pair_id = fx.json(&body)["pair_id"].as_str().unwrap().to_string();
    fx.post_from(
        ip,
        "/api/peer/pair/reveal",
        &format!(r#"{{"pair_id":"{pair_id}","nonce":"{nonce_a}"}}"#),
    );
    crate::peers::decline_prompt_ctx(&fx.ctx, &pair_id).unwrap();

    let (status, _, body) = fx.post_from(ip, "/api/peer/pair/poll", &format!(r#"{{"pair_id":"{pair_id}"}}"#));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fx.json(&body)["state"], "declined");
    // Reported once, then gone.
    assert_eq!(
        fx.post_from(ip, "/api/peer/pair/poll", &format!(r#"{{"pair_id":"{pair_id}"}}"#)).0,
        StatusCode::NOT_FOUND
    );
}

#[test]
fn pair_request_rejects_malformed_input() {
    let fx = http_fixture("123456");
    let good = commit_of("N");
    for body in [
        r#"{"device_id":"","name":"x","commit":"C","out_token":"T"}"#.to_string(),
        format!(r#"{{"device_id":"has space","commit":"{good}","out_token":"T"}}"#),
        format!(r#"{{"device_id":"ok","commit":"tooshort","out_token":"T"}}"#),
        format!(r#"{{"device_id":"ok","commit":"{good}","out_token":""}}"#),
        format!(r#"{{"device_id":"ok","commit":"{good}","out_token":"bad token"}}"#),
    ] {
        let (status, _, _) = fx.post_from([192, 168, 1, 60], "/api/peer/pair/request", &body);
        assert_eq!(status, StatusCode::BAD_REQUEST, "accepted: {body}");
        assert!(fx.ctx.pending_pairs.lock().unwrap().is_empty());
    }
}

#[test]
fn a_device_cannot_pair_with_itself() {
    let fx = http_fixture("123456");
    let commit = commit_of("N1N1N1N1N1N1N1N1N1N1N1N1N1");
    let body = format!(
        r#"{{"device_id":"hostdevice1","name":"Me","commit":"{commit}","out_token":"TOK"}}"#
    );
    let (status, _, _) = fx.post_from([127, 0, 0, 1], "/api/peer/pair/request", &body);
    assert_eq!(status, StatusCode::CONFLICT);
}

#[test]
fn pair_routes_are_404_when_peering_is_off() {
    let fx = fixture_with(FixtureOpts {
        peering_enabled: false,
        ..Default::default()
    });
    let commit = commit_of("N1N1N1N1N1N1N1N1N1N1N1N1N1");
    for (uri, body) in [
        ("/api/peer/pair/request", pair_request_body(&commit)),
        ("/api/peer/pair/reveal", r#"{"pair_id":"x","nonce":"y"}"#.to_string()),
        ("/api/peer/pair/poll", r#"{"pair_id":"x"}"#.to_string()),
    ] {
        let (status, _, _) = fx.post_from([192, 168, 1, 50], uri, &body);
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    let (status, _, _) = fx.get("/api/peer/hello", None);
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Peer auth
// ===========================================================================

/// The refusal must be indistinguishable from "peering is off". A different
/// status or body would make the endpoint an oracle for which tokens exist.
#[test]
fn an_unknown_peer_token_is_indistinguishable_from_peering_being_off() {
    let known = fixture_with(FixtureOpts {
        peers: vec![test_peer("dev1", "INTOKENINTOKENINTOKENINT01")],
        ..Default::default()
    });
    let (status_a, _, body_a) = known.post_as_peer("/api/peer/offer", "GARBAGEGARBAGEGARBAGEGAR01", r#"{"files":[]}"#);

    let off = fixture_with(FixtureOpts {
        peering_enabled: false,
        ..Default::default()
    });
    let (status_b, _, body_b) = off.post_as_peer("/api/peer/offer", "INTOKENINTOKENINTOKENINT01", r#"{"files":[]}"#);

    assert_eq!(status_a, StatusCode::NOT_FOUND);
    assert_eq!(status_b, StatusCode::NOT_FOUND);
    // Byte-identical, not merely both 404.
    assert_eq!(body_a, body_b);
}

/// The regression test for the sharpest edge in this feature. The extractor's
/// "PIN disabled" branch admits ANY request as Full, so a blocked peer would
/// sail straight past its own block the moment the user turned the PIN off --
/// with no symptom until someone went looking.
#[test]
fn a_blocked_peer_is_refused_even_when_the_pin_is_disabled() {
    let mut blocked = test_peer("dev1", "INTOKENINTOKANINTOKENINT01");
    blocked.blocked = true;

    let fx = fixture_with(FixtureOpts {
        pin: String::new(), // PIN OFF
        peers: vec![blocked],
        ..Default::default()
    });

    // Sanity: with the PIN off an anonymous browser really is admitted.
    let (status, _, _) = fx.get("/api/list?share=shr1&path=", None);
    assert_eq!(status, StatusCode::OK);

    // The blocked device is not.
    let (status, _, _) = fx.get_as_peer("/api/list?share=shr1&path=", "INTOKENINTOKANINTOKENINT01");
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = fx.post_as_peer("/api/peer/offer", "INTOKENINTOKANINTOKENINT01", r#"{"files":[]}"#);
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test]
fn a_blocked_peer_is_refused_with_the_pin_enabled_too() {
    let mut blocked = test_peer("dev1", "INTOKENINTOKENINTOKENINT01");
    blocked.blocked = true;
    let fx = fixture_with(FixtureOpts {
        peers: vec![blocked],
        ..Default::default()
    });
    let (status, _, _) = fx.get_as_peer("/api/list?share=shr1&path=", "INTOKENINTOKENINTOKENINT01");
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// A pair token must never be looked up in the session map, and a session
/// cookie must never reach a peer route.
#[test]
fn credentials_do_not_cross_between_the_two_paths() {
    let fx = fixture_with(FixtureOpts {
        peers: vec![test_peer("dev1", "INTOKENINTOKENINTOKENINT01")],
        ..Default::default()
    });

    // A pair token presented as a cookie is not a session.
    let (status, _, _) = fx.get(
        "/api/list?share=shr1&path=",
        Some("lanshare_sid=INTOKENINTOKENINTOKENINT01"),
    );
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A session cookie cannot reach a peer route.
    let cookie = fx.login("123456");
    let request = fx
        .build("/api/peer/offer", Some(&cookie), None)
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"files":[]}"#))
        .unwrap();
    let (status, _, _) = fx.send(request);
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A bearer token that is not a peer at all must still reach the session path,
/// or the documented curl-testability of session tokens breaks.
#[test]
fn a_non_peer_bearer_token_still_reaches_the_session_path() {
    let fx = fixture_with(FixtureOpts {
        peers: vec![test_peer("dev1", "INTOKENINTOKENINTOKENINT01")],
        ..Default::default()
    });
    let cookie = fx.login("123456");
    let session_token = cookie.trim_start_matches("lanshare_sid=").to_string();

    let (status, _, _) = fx.get_as_peer("/api/list?share=shr1&path=", &session_token);
    assert_eq!(status, StatusCode::OK);
}

#[test]
fn peer_token_lookup_is_order_independent() {
    let peers: Vec<Peer> = (0..50)
        .map(|i| test_peer(&format!("dev{i}"), &format!("TOKEN{i:021}")))
        .collect();
    let fx = fixture_with(FixtureOpts {
        peers,
        ..Default::default()
    });
    for i in [0usize, 25, 49] {
        let (status, _, _) = fx.get_as_peer("/api/shares", &format!("TOKEN{i:021}"));
        assert_eq!(status, StatusCode::OK, "peer {i} was not found");
    }
}

// ===========================================================================
// Peer browse
// ===========================================================================

#[test]
fn a_peer_browses_without_a_pin() {
    let fx = fixture_with(FixtureOpts {
        peers: vec![test_peer("dev1", "INTOKENINTOKENINTOKENINT01")],
        ..Default::default()
    });
    let token = "INTOKENINTOKENINTOKENINT01";

    let (status, _, body) = fx.get_as_peer("/api/shares", token);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fx.json(&body)[0]["id"], "shr1");

    let (status, _, body) = fx.get_as_peer("/api/list?share=shr1&path=", token);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fx.json(&body)["share_name"], "Photos");

    let (status, _, body) = fx.get_as_peer("/files/shr1/photo.jpg", token);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"0123456789");
}

#[test]
fn peer_browse_respects_its_own_switch() {
    let fx = fixture_with(FixtureOpts {
        peers: vec![test_peer("dev1", "INTOKENINTOKENINTOKENINT01")],
        peer_browse_enabled: false,
        ..Default::default()
    });
    let token = "INTOKENINTOKENINTOKENINT01";

    let (status, _, _) = fx.get_as_peer("/api/list?share=shr1&path=", token);
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Sending still works -- browse and receive are separate permissions.
    let (status, _, _) = fx.post_as_peer(
        "/api/peer/offer",
        token,
        r#"{"files":[{"name":"a.txt","size":1}]}"#,
    );
    assert_eq!(status, StatusCode::OK);
}

/// The traversal table replayed with a bearer instead of a cookie. This is what
/// proves the peer path really is the same code, not a parallel one that could
/// drift.
#[test]
fn a_peer_cannot_escape_a_share() {
    let fx = fixture_with(FixtureOpts {
        peers: vec![test_peer("dev1", "INTOKENINTOKENINTOKENINT01")],
        ..Default::default()
    });
    let token = "INTOKENINTOKENINTOKENINT01";

    for uri in [
        "/files/shr1/..%2F..%2Fsecret.txt",
        "/files/shr1/..%2Fsecret.txt",
        "/download/shr1/..%2F..%2Fsecret.txt",
    ] {
        let (status, _, _) = fx.get_as_peer(uri, token);
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND,
            "{uri} returned {status}"
        );
    }
    let (status, _, _) = fx.get_as_peer("/api/list?share=shr1&path=..%2F..", token);
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[test]
fn a_disabled_share_is_invisible_to_a_peer() {
    let fx = fixture_with(FixtureOpts {
        peers: vec![test_peer("dev1", "INTOKENINTOKENINTOKENINT01")],
        ..Default::default()
    });
    fx.ctx.shares.write().unwrap().shares[0].cfg.enabled = false;

    let (status, _, _) = fx.get_as_peer("/api/list?share=shr1&path=", "INTOKENINTOKENINTOKENINT01");
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test]
fn peer_activity_is_logged_under_the_peer_name() {
    let fx = fixture_with(FixtureOpts {
        peers: vec![test_peer("dev1", "INTOKENINTOKENINTOKENINT01")],
        ..Default::default()
    });
    fx.get_as_peer("/api/list?share=shr1&path=", "INTOKENINTOKENINTOKENINT01");

    let log = fx.ctx.activity.lock().unwrap();
    let newest = log.front().expect("nothing was logged");
    assert_eq!(newest.user_agent, "peer:Peer dev1");
}

// ===========================================================================
// Offers and receiving files
// ===========================================================================

fn peer_fixture() -> (HttpFixture, &'static str) {
    let token = "INTOKENINTOKENINTOKENINT01";
    let fx = fixture_with(FixtureOpts {
        peers: vec![test_peer("dev1", token)],
        ..Default::default()
    });
    (fx, token)
}

/// A rejected filename must fail the WHOLE offer with nothing recorded, so the
/// human is never shown a prompt whose contents differ from what would land.
#[test]
fn an_offer_with_a_bad_filename_is_refused_entirely() {
    let (fx, token) = peer_fixture();
    for name in [
        "../../evil.exe",
        "..\\evil.exe",
        "/etc/passwd",
        "C:\\x.txt",
        "a/b.txt",
        "con.txt",
        "trailing.",
        "nul",
        "x:y",
        "..",
    ] {
        let body = format!(
            r#"{{"files":[{{"name":"ok.txt","size":1}},{{"name":"{}","size":1}}]}}"#,
            name.replace('\\', "\\\\")
        );
        let (status, _, out) = fx.post_as_peer("/api/peer/offer", token, &body);
        assert_eq!(status, StatusCode::BAD_REQUEST, "accepted {name:?}");
        assert_eq!(fx.json(&out)["error"], "bad_filename");
        assert!(
            fx.ctx.offers.lock().unwrap().is_empty(),
            "a partial offer was recorded for {name:?}"
        );
    }
}

#[test]
fn an_empty_or_oversized_offer_is_refused() {
    let (fx, token) = peer_fixture();
    assert_eq!(
        fx.post_as_peer("/api/peer/offer", token, r#"{"files":[]}"#).0,
        StatusCode::BAD_REQUEST
    );

    fx.ctx.settings.write().unwrap().max_offer_bytes = 10;
    let (status, _, out) = fx.post_as_peer(
        "/api/peer/offer",
        token,
        r#"{"files":[{"name":"big.bin","size":1000}]}"#,
    );
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(fx.json(&out)["error"], "too_large");
}

#[test]
fn a_second_offer_from_the_same_peer_is_refused() {
    let (fx, token) = peer_fixture();
    let body = r#"{"files":[{"name":"a.txt","size":1}]}"#;
    assert_eq!(fx.post_as_peer("/api/peer/offer", token, body).0, StatusCode::OK);
    let (status, _, out) = fx.post_as_peer("/api/peer/offer", token, body);
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(fx.json(&out)["error"], "offer_in_progress");
}

#[test]
fn an_auto_accept_peer_skips_the_prompt() {
    let mut peer = test_peer("dev1", "INTOKENINTOKENINTOKENINT01");
    peer.auto_accept = true;
    let fx = fixture_with(FixtureOpts {
        peers: vec![peer],
        ..Default::default()
    });

    let (status, _, body) = fx.post_as_peer(
        "/api/peer/offer",
        "INTOKENINTOKENINTOKENINT01",
        r#"{"files":[{"name":"a.txt","size":1}]}"#,
    );
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fx.json(&body)["state"], "accepted");
    // Nothing to ask the user about.
    assert!(crate::peers::list_offers_ctx(&fx.ctx).is_empty());
}

fn make_offer(fx: &HttpFixture, token: &str, name: &str, size: usize) -> String {
    let body = format!(r#"{{"files":[{{"name":"{name}","size":{size}}}]}}"#);
    let (status, _, out) = fx.post_as_peer("/api/peer/offer", token, &body);
    assert_eq!(status, StatusCode::OK);
    fx.json(&out)["offerId"].as_str().unwrap().to_string()
}

fn put_file(
    fx: &HttpFixture,
    token: &str,
    offer_id: &str,
    index: usize,
    body: &[u8],
    declared: Option<usize>,
) -> (StatusCode, Vec<u8>) {
    let mut builder = fx
        .build(&format!("/api/peer/file/{offer_id}/{index}"), None, None)
        .method("PUT")
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(len) = declared {
        builder = builder.header(header::CONTENT_LENGTH, len.to_string());
    }
    let (status, _, out) = fx.send(builder.body(Body::from(body.to_vec())).unwrap());
    (status, out)
}

#[test]
fn a_file_cannot_be_pushed_before_acceptance() {
    let (fx, token) = peer_fixture();
    let offer_id = make_offer(&fx, token, "photo.jpg", 5);
    let (status, out) = put_file(&fx, token, &offer_id, 0, b"hello", Some(5));
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(fx.json(&out)["error"], "not_accepted");
    assert!(!fx.receive_dir.join("photo.jpg").exists());
}

#[test]
fn a_file_cannot_be_pushed_after_a_decline() {
    let (fx, token) = peer_fixture();
    let offer_id = make_offer(&fx, token, "photo.jpg", 5);
    crate::peers::set_offer_state_ctx(&fx.ctx, &offer_id, false).unwrap();
    let (status, _) = put_file(&fx, token, &offer_id, 0, b"hello", Some(5));
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(!fx.receive_dir.join("photo.jpg").exists());
}

#[test]
fn an_accepted_file_lands_in_the_receive_folder() {
    let (fx, token) = peer_fixture();
    let offer_id = make_offer(&fx, token, "photo.jpg", 5);
    crate::peers::set_offer_state_ctx(&fx.ctx, &offer_id, true).unwrap();

    let (status, out) = put_file(&fx, token, &offer_id, 0, b"hello", Some(5));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fx.json(&out)["savedAs"], "photo.jpg");
    assert_eq!(fs::read_to_string(fx.receive_dir.join("photo.jpg")).unwrap(), "hello");
}

#[test]
fn content_length_is_required_and_must_match() {
    let (fx, token) = peer_fixture();
    let offer_id = make_offer(&fx, token, "photo.jpg", 5);
    crate::peers::set_offer_state_ctx(&fx.ctx, &offer_id, true).unwrap();

    let (status, _) = put_file(&fx, token, &offer_id, 0, b"hello", None);
    assert_eq!(status, StatusCode::LENGTH_REQUIRED);

    let (status, out) = put_file(&fx, token, &offer_id, 0, b"hello", Some(99));
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(fx.json(&out)["error"], "size_mismatch");
}

/// A truncated transfer must leave NOTHING -- neither the file nor the temp.
/// Renaming a short body into place would present corruption as success.
#[test]
fn a_short_body_leaves_no_file_and_no_part() {
    let (fx, token) = peer_fixture();
    let offer_id = make_offer(&fx, token, "photo.jpg", 100);
    crate::peers::set_offer_state_ctx(&fx.ctx, &offer_id, true).unwrap();

    let (status, _) = put_file(&fx, token, &offer_id, 0, b"short", Some(100));
    assert_ne!(status, StatusCode::OK);
    assert!(!fx.receive_dir.join("photo.jpg").exists());
    let leftovers: Vec<_> = fs::read_dir(&fx.receive_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
}

#[test]
fn an_oversized_body_is_refused_and_cleaned_up() {
    let (fx, token) = peer_fixture();
    let offer_id = make_offer(&fx, token, "photo.jpg", 5);
    crate::peers::set_offer_state_ctx(&fx.ctx, &offer_id, true).unwrap();

    let (status, _) = put_file(&fx, token, &offer_id, 0, &vec![b'x'; 5000], Some(5));
    assert_ne!(status, StatusCode::OK);
    assert!(!fx.receive_dir.join("photo.jpg").exists());
    assert_eq!(fs::read_dir(&fx.receive_dir).unwrap().count(), 0);
}

/// A receiver must never be able to clobber a file the host already has.
#[test]
fn receiving_never_overwrites() {
    let (fx, token) = peer_fixture();
    write_file(&fx.receive_dir.join("photo.jpg"), "ORIGINAL");

    let offer_id = make_offer(&fx, token, "photo.jpg", 3);
    crate::peers::set_offer_state_ctx(&fx.ctx, &offer_id, true).unwrap();
    let (status, out) = put_file(&fx, token, &offer_id, 0, b"new", Some(3));

    assert_eq!(status, StatusCode::OK);
    assert_eq!(fx.json(&out)["savedAs"], "photo (2).jpg");
    assert_eq!(
        fs::read_to_string(fx.receive_dir.join("photo.jpg")).unwrap(),
        "ORIGINAL"
    );
    assert_eq!(
        fs::read_to_string(fx.receive_dir.join("photo (2).jpg")).unwrap(),
        "new"
    );
}

#[test]
fn the_same_index_cannot_be_sent_twice() {
    let (fx, token) = peer_fixture();
    let body = r#"{"files":[{"name":"a.txt","size":1},{"name":"b.txt","size":1}]}"#;
    let (_, _, out) = fx.post_as_peer("/api/peer/offer", token, body);
    let offer_id = fx.json(&out)["offerId"].as_str().unwrap().to_string();
    crate::peers::set_offer_state_ctx(&fx.ctx, &offer_id, true).unwrap();

    assert_eq!(put_file(&fx, token, &offer_id, 0, b"a", Some(1)).0, StatusCode::OK);
    let (status, out) = put_file(&fx, token, &offer_id, 0, b"a", Some(1));
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(fx.json(&out)["error"], "already_sent");
}

#[test]
fn a_peer_cannot_push_into_another_peers_offer() {
    let token_a = "INTOKENAAAAAAAAAAAAAAAAA01";
    let token_b = "INTOKENBBBBBBBBBBBBBBBBB01";
    let fx = fixture_with(FixtureOpts {
        peers: vec![test_peer("deva", token_a), test_peer("devb", token_b)],
        ..Default::default()
    });

    let offer_id = make_offer(&fx, token_a, "photo.jpg", 3);
    crate::peers::set_offer_state_ctx(&fx.ctx, &offer_id, true).unwrap();

    let (status, _) = put_file(&fx, token_b, &offer_id, 0, b"new", Some(3));
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!fx.receive_dir.join("photo.jpg").exists());
}

#[test]
fn an_unknown_file_index_is_refused() {
    let (fx, token) = peer_fixture();
    let offer_id = make_offer(&fx, token, "photo.jpg", 3);
    crate::peers::set_offer_state_ctx(&fx.ctx, &offer_id, true).unwrap();
    let (status, _) = put_file(&fx, token, &offer_id, 7, b"new", Some(3));
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test]
fn part_files_are_swept() {
    let base = tempdir_unique("parts");
    write_file(&base.join(format!("abc{}", crate::models::PART_SUFFIX)), "x");
    write_file(&base.join("keep.txt"), "x");

    let removed = crate::transfer::sweep_parts(&base, 0);
    assert_eq!(removed, 1);
    assert!(base.join("keep.txt").exists());
    let _ = fs::remove_dir_all(&base);
}

// ===========================================================================
// Beacons
// ===========================================================================

use crate::discovery::Beacon;

fn beacon(name: &str) -> Beacon {
    Beacon {
        alive: true,
        device_id: "abc123".to_string(),
        port: 8080,
        platform: "windows".to_string(),
        name: name.to_string(),
    }
}

#[test]
fn beacons_round_trip() {
    for name in [
        "Rajan's PC",
        "Living Room  TV",
        "rÃ©seau",
        "100% done",
        "æ—¥æœ¬èªž",
        "",
        "a b c d e",
    ] {
        let original = beacon(name);
        let decoded = Beacon::decode(&original.encode()).expect("failed to decode {name}");
        // The name is cleaned on the way in, so compare against the cleaned form.
        assert_eq!(decoded.device_id, original.device_id);
        assert_eq!(decoded.port, original.port);
        assert_eq!(decoded.platform, original.platform);
        assert_eq!(decoded.name, crate::config::clean_device_name(name));
    }
}

#[test]
fn a_goodbye_beacon_round_trips() {
    let mut b = beacon("Bye");
    b.alive = false;
    assert!(!Beacon::decode(&b.encode()).unwrap().alive);
}

/// Everything here arrives from an unauthenticated stranger on the subnet.
#[test]
fn malformed_beacons_are_rejected() {
    for raw in [
        b"".as_slice(),
        b"LANSHARE/2 hi abc 1 windows eA",
        b"LANSHAR/1 hi abc 1 windows eA",
        b"LANSHARE/1 hi abc 1 windows",
        b"LANSHARE/1 hi abc 1 windows eA extra",
        b"LANSHARE/1 wat abc 1 windows eA",
        b"LANSHARE/1 hi abc 0 windows eA",
        b"LANSHARE/1 hi abc 99999 windows eA",
        b"LANSHARE/1 hi  1 windows eA",
        b"LANSHARE/1 hi ab!c 1 windows eA",
        b"LANSHARE/1 hi abc 1 WINDOWS eA",
        b"LANSHARE/1 hi abc 1 windows !!!",
        b"\xff\xfe\x00 garbage",
    ] {
        assert!(
            Beacon::decode(raw).is_none(),
            "accepted: {:?}",
            String::from_utf8_lossy(raw)
        );
    }
}

#[test]
fn an_oversized_beacon_is_rejected() {
    let mut raw = b"LANSHARE/1 hi abc 1 windows ".to_vec();
    raw.extend(std::iter::repeat(b'A').take(crate::models::BEACON_MAX_BYTES));
    assert!(Beacon::decode(&raw).is_none());
}

/// A remote device does not get to reorder our UI with a bidi override, nor
/// push a 4 KB name into the device list.
#[test]
fn beacon_names_are_cleaned_and_clamped() {
    let hostile = beacon("photo\u{202E}gnp.exe\u{0007}");
    let decoded = Beacon::decode(&hostile.encode()).unwrap();
    assert!(!decoded.name.contains('\u{202E}'));
    assert!(!decoded.name.contains('\u{0007}'));

    // A pathological name must not push the packet past `decode`'s ceiling --
    // that would make this device silently undiscoverable rather than merely
    // oddly named, with no error on either machine.
    let long = beacon(&"x".repeat(500));
    let raw = long.encode();
    assert!(
        raw.len() <= crate::models::BEACON_MAX_BYTES,
        "encode emitted {} bytes, over its own limit",
        raw.len()
    );
    let decoded = Beacon::decode(&raw).expect("our own beacon was rejected");
    assert!(decoded.name.chars().count() <= crate::models::DEVICE_NAME_MAX);
}

#[test]
fn our_own_beacon_is_ignored_but_marks_the_socket_healthy() {
    let fx = http_fixture("123456");
    let mut mine = beacon("Me");
    mine.device_id = "hostdevice1".to_string(); // the fixture's own id

    crate::discovery::apply_beacon(&fx.ctx, &mine, "192.168.1.10".parse().unwrap());

    assert!(fx.ctx.discovered.lock().unwrap().is_empty());
    // Both halves matter: seeing ourselves is the inbound-path health signal.
    assert!(fx.ctx.discovery_self_seen_ms.load(std::sync::atomic::Ordering::Relaxed) > 0);
}

/// Filtering self-packets on source IP instead of device id would hide a
/// second instance on this machine -- the configuration we develop against.
#[test]
fn a_second_instance_on_our_own_ip_is_visible() {
    let fx = http_fixture("123456");
    let other = beacon("Second");
    crate::discovery::apply_beacon(&fx.ctx, &other, "127.0.0.1".parse().unwrap());
    assert_eq!(fx.ctx.discovered.lock().unwrap().len(), 1);
}

#[test]
fn beacons_merge_addresses_for_one_device() {
    let fx = http_fixture("123456");
    let b = beacon("Multi");
    crate::discovery::apply_beacon(&fx.ctx, &b, "192.168.1.5".parse().unwrap());
    crate::discovery::apply_beacon(&fx.ctx, &b, "10.0.0.5".parse().unwrap());

    let table = fx.ctx.discovered.lock().unwrap();
    assert_eq!(table.len(), 1, "a device on two interfaces became two rows");
    assert_eq!(table["abc123"].addresses.len(), 2);
}

#[test]
fn a_goodbye_marks_offline_without_removing_the_row() {
    let fx = http_fixture("123456");
    let mut b = beacon("Leaving");
    crate::discovery::apply_beacon(&fx.ctx, &b, "192.168.1.5".parse().unwrap());
    b.alive = false;
    crate::discovery::apply_beacon(&fx.ctx, &b, "192.168.1.5".parse().unwrap());

    let table = fx.ctx.discovered.lock().unwrap();
    // Still listed, so it does not vanish mid-glance -- just not online.
    assert_eq!(table.len(), 1);
    assert!(!table["abc123"].online(crate::utils::now_ms()));
}

#[test]
fn the_discovered_table_is_capped() {
    let fx = http_fixture("123456");
    for i in 0..(crate::models::DISCOVERED_CAP + 40) {
        let mut b = beacon("Flood");
        b.device_id = format!("dev{i:08}");
        crate::discovery::apply_beacon(&fx.ctx, &b, "192.168.1.5".parse().unwrap());
    }
    assert_eq!(
        fx.ctx.discovered.lock().unwrap().len(),
        crate::models::DISCOVERED_CAP
    );
}

#[test]
fn stale_devices_go_offline_then_are_evicted() {
    let fx = http_fixture("123456");
    crate::discovery::apply_beacon(&fx.ctx, &beacon("Old"), "192.168.1.5".parse().unwrap());

    let now = crate::utils::now_ms();
    {
        let mut table = fx.ctx.discovered.lock().unwrap();
        let entry = table.get_mut("abc123").unwrap();
        entry.last_seen_ms = now - (crate::models::PEER_OFFLINE_AFTER_MS + 1000);
        assert!(!entry.online(now));
    }
    // Offline but not yet evicted.
    crate::discovery::sweep(&fx.ctx);
    assert_eq!(fx.ctx.discovered.lock().unwrap().len(), 1);

    {
        let mut table = fx.ctx.discovered.lock().unwrap();
        table.get_mut("abc123").unwrap().last_seen_ms =
            now - (crate::models::PEER_EVICT_AFTER_MS + 1000);
    }
    crate::discovery::sweep(&fx.ctx);
    assert!(fx.ctx.discovered.lock().unwrap().is_empty());
}

/// A manually added device was typed in by hand, so forgetting it is data loss
/// rather than tidying.
#[test]
fn a_manually_added_device_survives_the_sweep() {
    let fx = http_fixture("123456");
    crate::discovery::apply_beacon(&fx.ctx, &beacon("Manual"), "192.168.1.5".parse().unwrap());
    {
        let mut table = fx.ctx.discovered.lock().unwrap();
        let entry = table.get_mut("abc123").unwrap();
        entry.manual = true;
        entry.last_seen_ms = 1;
    }
    crate::discovery::sweep(&fx.ctx);
    assert_eq!(fx.ctx.discovered.lock().unwrap().len(), 1);
}

#[test]
fn directed_broadcast_is_derived_from_the_netmask() {
    use std::net::Ipv4Addr;
    let cases = [
        ([192, 168, 1, 77], [255, 255, 255, 0], [192, 168, 1, 255]),
        ([10, 0, 0, 5], [255, 0, 0, 0], [10, 255, 255, 255]),
        ([172, 16, 4, 9], [255, 255, 0, 0], [172, 16, 255, 255]),
        // A zero mask cannot describe a subnet; fall back to global broadcast.
        ([192, 168, 1, 1], [0, 0, 0, 0], [255, 255, 255, 255]),
    ];
    for (ip, mask, expect) in cases {
        assert_eq!(
            crate::discovery::directed_broadcast(Ipv4Addr::from(ip), Ipv4Addr::from(mask)),
            Ipv4Addr::from(expect),
            "{ip:?}/{mask:?}"
        );
    }
}

#[test]
fn manual_addresses_are_parsed_or_rejected() {
    use crate::discovery::parse_manual_address;
    assert_eq!(
        parse_manual_address("192.168.1.5", 8080).unwrap(),
        ("192.168.1.5".parse().unwrap(), 8080)
    );
    assert_eq!(
        parse_manual_address(" 192.168.1.5:9000 ", 8080).unwrap(),
        ("192.168.1.5".parse().unwrap(), 9000)
    );
    for bad in [
        "",
        "not an ip",
        "999.1.1.1",
        "192.168.1.1:0",
        "192.168.1.1:99999",
        "http://192.168.1.1",
    ] {
        assert!(parse_manual_address(bad, 8080).is_err(), "accepted {bad:?}");
    }
}

#[test]
fn discovery_health_distinguishes_a_block_from_an_empty_network() {
    use crate::discovery::health_from;
    let now = crate::utils::now_ms();

    // Too early to conclude anything.
    assert_eq!(health_from(now - 5_000, 0, true), "ok");
    // Announcing a while, never heard ourselves.
    assert_eq!(health_from(now - 60_000, now - 60_000, true), "inbound_likely_blocked");
    // We hear ourselves, so inbound works -- but nobody else is out there.
    assert_eq!(health_from(now - 60_000, now - 1_000, true), "nothing_heard");
    assert_eq!(health_from(now - 60_000, now - 1_000, false), "ok");
    // Never started.
    assert_eq!(health_from(0, 0, true), "ok");
}

// ===========================================================================
// Peer config
// ===========================================================================

#[test]
fn normalize_backfills_a_device_id_and_keeps_it_stable() {
    let mut config = AppConfig::default();
    assert!(config.device_id.is_empty());

    config::normalize(&mut config);
    let first = config.device_id.clone();
    assert!(!first.is_empty());

    // Stable across every later normalize: every paired device stores this id
    // and it feeds the pairing hash, so regenerating it would silently break
    // every existing pairing.
    config::normalize(&mut config);
    assert_eq!(config.device_id, first);
}

#[test]
fn normalize_drops_half_paired_peers() {
    let mut config = AppConfig::default();
    let mut good = test_peer("good", "INTOKEN");
    good.added_ms = 2;
    let mut no_in = test_peer("noin", "");
    no_in.added_ms = 3;
    let mut no_out = test_peer("noout", "INTOKEN");
    no_out.out_token = String::new();
    config.peers = vec![good, no_in, no_out];

    config::normalize(&mut config);

    let ids: Vec<&str> = config.peers.iter().map(|p| p.device_id.as_str()).collect();
    assert_eq!(ids, vec!["good"]);
}

#[test]
fn normalize_dedupes_peers_keeping_the_newest_pairing() {
    let mut config = AppConfig::default();
    let mut old = test_peer("dev1", "OLDTOKEN");
    old.added_ms = 100;
    let mut new = test_peer("dev1", "NEWTOKEN");
    new.added_ms = 200;
    config.peers = vec![old, new];

    config::normalize(&mut config);

    assert_eq!(config.peers.len(), 1);
    assert_eq!(config.peers[0].in_token, "NEWTOKEN");
}

#[test]
fn normalize_clamps_the_discovery_port() {
    let mut config = AppConfig::default();
    for bad in [0u16, 80, 1023] {
        config.discovery_port = bad;
        config::normalize(&mut config);
        assert_eq!(config.discovery_port, crate::models::DEFAULT_DISCOVERY_PORT);
    }
    // Colliding with the HTTP port is legal for UDP but confusing to explain.
    config.discovery_port = config.port;
    config::normalize(&mut config);
    assert_eq!(config.discovery_port, crate::models::DEFAULT_DISCOVERY_PORT);
}

#[test]
fn clean_device_name_strips_controls_and_bidi_overrides() {
    use crate::config::clean_device_name;
    // Escaping alone does not stop these -- they are legal characters that
    // reorder what the eye sees.
    assert_eq!(clean_device_name("photo\u{202E}gnp.exe"), "photognp.exe");
    assert_eq!(clean_device_name("a\u{0007}b"), "ab");
    assert_eq!(clean_device_name("  spaced  "), "spaced");
    assert_eq!(clean_device_name("\u{2066}sneaky\u{2069}"), "sneaky");
    assert!(clean_device_name(&"x".repeat(200)).chars().count() <= crate::models::DEVICE_NAME_MAX);
    assert_eq!(clean_device_name("Rajan's PC"), "Rajan's PC");
}

#[test]
fn peer_defaults_are_safe() {
    let config = AppConfig::default();
    assert!(config.peering_enabled);
    assert!(config.discoverable);
    assert!(config.peer_browse_enabled);
    assert!(config.peers.is_empty());
    assert!(config.receive_dir.is_none());

    let peer = test_peer("x", "y");
    // A newly paired device must never be silently trusted with unattended
    // receipt -- that is a decision the user makes afterwards, per device.
    assert!(!peer.auto_accept);
    assert!(!peer.blocked);
}

#[test]
fn an_empty_peer_token_never_matches() {
    let registry = crate::models::PeerRegistry {
        peers: vec![Peer {
            in_token: String::new(),
            ..test_peer("dev1", "")
        }],
    };
    // A hand-edited config with a blank token must not become a skeleton key.
    assert!(registry.by_in_token("").is_none());
    assert!(registry.by_in_token("anything").is_none());
}
