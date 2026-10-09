fn main() {
    use std::{collections::HashSet, env, fs, path::PathBuf};

    println!("cargo:rerun-if-changed=assets/packs.toml");
    let text = fs::read_to_string("assets/packs.toml").expect("read embedded templates");
    let bundle: toml::Value = toml::from_str(&text).expect("parse embedded templates");
    let mut paths = HashSet::new();
    let mut output = String::from("static FILES: &[PackFile] = &[\n");
    for pack in bundle["pack"].as_array().expect("template list") {
        let path = pack["path"].as_str().expect("template path");
        let scope = pack["scope"].as_str().expect("template scope");
        assert!(paths.insert(path), "duplicate template path");
        assert!(["Lol", "SteamGlobal", "SteamAccount"].contains(&scope));
        let mut content = pack["content"].as_str().expect("template content").to_owned();
        if pack["crlf"].as_bool().unwrap_or(false) { content = content.replace('\n', "\r\n"); }
        output.push_str(&format!("PackFile {{ bytes: {content:?}.as_bytes(), rel: {path:?}, scope: PackScope::{scope} }},\n"));
    }
    output.push_str("];\n");
    fs::write(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("packs.rs"), output).expect("embed templates");
    tauri_build::build();
}
