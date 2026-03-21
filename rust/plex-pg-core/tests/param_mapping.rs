use plex_pg_core::translate;

#[test]
fn metadata_items_param_names_match_expected_positions() {
    let sql = "INSERT INTO metadata_items (library_section_id, parent_id, metadata_type, guid, edition_title, slug, hash, media_item_count, title, title_sort) \
               VALUES (:U1, :U2, :U3, :U4, :U5, :U6, :U7, :U8, :U9, :U10)";
    let r = translate(sql).expect("translate");
    assert_eq!(r.param_names.len(), 10);
    assert_eq!(r.param_names[0].as_deref(), Some("U1"));
    assert_eq!(r.param_names[3].as_deref(), Some("U4"));
    assert_eq!(r.param_names[8].as_deref(), Some("U9"));
    assert_eq!(r.param_names[9].as_deref(), Some("U10"));
}
