fn main() {
    for asset in ["index.html", "app.js", "style.css"] {
        let path = format!("../../clients/hive-web/dist/{asset}");
        // Cargo must embed the current bundle, but must never install packages implicitly.
        println!("cargo:rerun-if-changed={path}");
        assert!(
            std::path::Path::new(&path).is_file(),
            "Missing browser asset {path}. From the repository root run `npm ci --prefix clients/hive-web` then `npm run build --prefix clients/hive-web` before Cargo."
        );
    }
}
