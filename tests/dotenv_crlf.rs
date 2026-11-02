//! Documenta que o `dotenvy` tolera `.env` com CRLF (editado no Windows): o
//! valor não pode carregar `\r` no fim. Se este teste quebrar numa troca de
//! versão, o loader precisa voltar a tirar o `\r` por conta própria.

#[test]
fn dotenvy_strips_cr_from_values() {
    let path = std::env::temp_dir().join(format!("openkax-crlf-{}.env", std::process::id()));
    std::fs::write(
        &path,
        "KAX_A=abc\r\nKAX_B=\"quoted\"\r\n# comentário\r\nKAX_C='single'\r\nKAX_D=\r\nKAX_E=fim",
    )
    .unwrap();
    let got: Vec<(String, String)> = dotenvy::from_path_iter(&path)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let _ = std::fs::remove_file(&path);
    let get = |k: &str| got.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    assert_eq!(get("KAX_A"), Some("abc"));
    assert_eq!(get("KAX_B"), Some("quoted"));
    assert_eq!(get("KAX_C"), Some("single"));
    assert_eq!(get("KAX_D"), Some(""));
    assert_eq!(get("KAX_E"), Some("fim"));
    assert!(
        got.iter().all(|(k, v)| !k.contains('\r') && !v.contains('\r')),
        "\\r vazou para nome ou valor"
    );
}
