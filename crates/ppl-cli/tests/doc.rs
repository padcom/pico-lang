//! `ppl doc` (PLAN P10.9b): HTML pages from doc comments, for the standard library (`--std`)
//! and for a project's package (its `index.ppl`).

use std::path::Path;
use std::process::Command;

fn ppl(args: &[&str], dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ppl"))
        .args(args)
        .current_dir(dir)
        .env("PPL_CACHE_DIR", dir.join("cache"))
        .output()
        .unwrap()
}

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ppl-doc-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn standard_library() {
    let dir = temp("std");
    let out = ppl(&["doc", "--std", "--out", "site"], &dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let site = dir.join("site");
    let index = read(&site.join("index.html"));
    for page in [
        "std-path.html",
        "core.html",
        "type-Array.html",
        "type-String.html",
    ] {
        assert!(
            index.contains(&format!("href=\"{page}\"")),
            "{page} not listed:\n{index}"
        );
    }
    // A declaration: its signature as written, its doc comment rendered from Markdown.
    let path = read(&site.join("std-path.html"));
    assert!(path.contains("<h2 id=\"join\">"), "{path}");
    assert!(
        path.contains("function join(...parts: string[]): string"),
        "{path}"
    );
    assert!(
        path.contains("<code>join(\"a\", \"../b\", \"c\")</code>"),
        "{path}"
    );
    assert!(path.contains("href=\"index.html\""), "{path}");
    // A built-in type's methods (from the core library's `__Array_map(xs, f)`), the receiver
    // dropped, and type names linked to their pages.
    let array = read(&site.join("type-Array.html"));
    assert!(array.contains("<h2 id=\"map\">"), "{array}");
    assert!(
        array.contains("map&lt;T, U, E extends <a href=\"core.html#Error\">Error</a>&gt;(f: "),
        "{array}"
    );
    assert!(!array.contains("__Array"), "{array}");
    // A class: its public members.
    let collections = read(&site.join("std-collections.html"));
    assert!(
        collections.contains("class Deque&lt;T&gt;"),
        "{collections}"
    );
    assert!(
        collections.contains("<h3 id=\"pushBack\">"),
        "{collections}"
    );
    // A module whose export list renames the core library's declarations
    // (`export { __ClassDecl as ClassDecl }`): documented under the public names.
    let meta = read(&site.join("std-meta.html"));
    assert!(meta.contains("<h2 id=\"ClassDecl\">"), "{meta}");
    assert!(meta.contains("class ClassDecl"), "{meta}");
    assert!(meta.contains("<h3 id=\"addMethod\">"), "{meta}");
    assert!(!meta.contains("__ClassDecl"), "{meta}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn project_api() {
    let dir = temp("project");
    std::fs::write(
        dir.join("project.json"),
        r#"{ "name": "acme/shapes", "version": "1.0.0" }"#,
    )
    .unwrap();
    let index = r#"/** A shape with an **area**. */
export interface Shape {
  /** The area, in square units. */
  area(): f64
}

/** A circle. */
export class Circle implements Shape {
  private secret: int = 0
  public constructor(public radius: f64) {}
  public area(): f64 { 3.14 * this.radius * this.radius }
}

/// The biggest of the shapes.
export function biggest(shapes: Shape[]): Shape? { null }

function internal(): void {}
"#;
    std::fs::write(dir.join("index.ppl"), index).unwrap();
    let out = ppl(&["doc"], &dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let page = read(&dir.join("target/doc/index-api.html"));
    assert!(
        page.contains("<p>A shape with an <strong>area</strong>.</p>"),
        "{page}"
    );
    assert!(page.contains("<h3 id=\"area\">"), "{page}");
    assert!(
        page.contains("class Circle implements <a href=\"index-api.html#Shape\">Shape</a>"),
        "{page}"
    );
    assert!(
        page.contains("function biggest(shapes: <a href=\"index-api.html#Shape\">Shape</a>[])"),
        "{page}"
    );
    assert!(!page.contains("secret"), "{page}");
    assert!(!page.contains("internal"), "{page}");
    // Without a package there is nothing to document.
    let bare = temp("bare");
    let out = ppl(&["doc"], &bare);
    assert!(!out.status.success());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&bare);
}
