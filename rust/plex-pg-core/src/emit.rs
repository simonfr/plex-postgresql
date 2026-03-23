/// Module: emit
///
/// Serialise a `Statement` back to SQL using the standard `Display` impl.
/// This relies on sqlparser's built-in `to_string()` / `Display` for Statement.
/// All quote-style and AST fixes are done by the earlier transform modules,
/// so a plain `to_string()` is sufficient to emit correct PostgreSQL SQL.
use sqlparser::ast::Statement;

/// Convert an AST `Statement` back to a SQL string.
pub fn emit(stmt: &Statement) -> String {
    stmt.to_string()
}

// ─── Integration tests ────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use crate::translate;

    #[test]
    fn subset_core__emit_select_with_placeholder() {
        let r = translate("SELECT id, title FROM metadata_items WHERE id = ?").unwrap();
        assert!(r.sql.contains("$1"));
        assert!(!r.sql.contains('?'));
        assert!(!r.sql.contains('`'));
    }

    #[test]
    fn subset_core__emit_insert_or_replace_full_pipeline() {
        let r = translate("INSERT OR REPLACE INTO settings(id, value) VALUES(?, ?)").unwrap();
        assert!(r.sql.to_uppercase().contains("ON CONFLICT"));
        assert!(r.sql.contains("$1") && r.sql.contains("$2"));
    }

    #[test]
    fn compat_backticks__emit_backticks_become_doublequotes() {
        let r = translate("SELECT `id`, `name` FROM `metadata_items`").unwrap();
        assert!(!r.sql.contains('`'));
        assert!(r.sql.contains('"'));
    }

    #[test]
    fn subset_ddl_lite__emit_create_table_if_not_exists() {
        let r = translate("CREATE TABLE foo (id INTEGER, name TEXT)").unwrap();
        assert!(r.sql.to_uppercase().contains("IF NOT EXISTS"));
    }

    #[test]
    fn subset_core__emit_ifnull_to_coalesce_pipeline() {
        let r = translate(
            "SELECT m.id, IFNULL(m.rating, 0) as rating FROM metadata_items m \
             WHERE m.library_section_id = :lib_id AND m.metadata_type = :type",
        )
        .unwrap();
        assert!(r.sql.to_uppercase().contains("COALESCE"));
        assert!(!r.sql.to_uppercase().contains("IFNULL"));
        assert!(r.sql.contains("$1") && r.sql.contains("$2"));
        assert_eq!(r.param_names.len(), 2);
        assert_eq!(r.param_names[0], Some("lib_id".to_string()));
        assert_eq!(r.param_names[1], Some("type".to_string()));
    }
}
