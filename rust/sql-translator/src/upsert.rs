/// Module: upsert
///
/// Rewrites SQLite INSERT OR REPLACE / INSERT OR IGNORE / REPLACE INTO
/// to PostgreSQL ON CONFLICT syntax:
///
///   INSERT OR REPLACE INTO t (cols) VALUES (vals)
///     → INSERT INTO t (cols) VALUES (vals) ON CONFLICT DO UPDATE SET col1=EXCLUDED.col1, ...
///
///   INSERT OR IGNORE INTO t (cols) VALUES (vals)
///     → INSERT INTO t (cols) VALUES (vals) ON CONFLICT DO NOTHING
///
///   REPLACE INTO t (cols) VALUES (vals)
///     → same as OR REPLACE
use sqlparser::ast::*;

pub fn transform(stmt: &mut Statement) {
    if let Statement::Insert(insert) = stmt {
        transform_insert(insert);
    }
}

fn transform_insert(insert: &mut Insert) {
    // Skip if already has ON CONFLICT clause
    if insert.on.is_some() {
        return;
    }

    // Look up conflict target columns from table name
    let table_name = match &insert.table {
        TableObject::TableName(name) => name
            .0
            .last()
            .and_then(|p| match p {
                ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                _ => None,
            })
            .unwrap_or_default(),
        _ => String::new(),
    };
    let conflict_cols = get_conflict_columns(&table_name);
    let conflict_target = conflict_cols
        .as_ref()
        .map(|cols| ConflictTarget::Columns(cols.iter().map(|c| Ident::new(*c)).collect()));

    // Handle both INSERT OR REPLACE and REPLACE INTO (replace_into flag)
    let is_replace = matches!(insert.or, Some(SqliteOnConflict::Replace)) || insert.replace_into;
    let is_ignore = matches!(insert.or, Some(SqliteOnConflict::Ignore));

    if is_replace {
        // INSERT OR REPLACE / REPLACE INTO → ON CONFLICT (target) DO UPDATE SET col=EXCLUDED.col, ...
        let columns = insert.columns.clone();
        let conflict_col_names: Vec<String> = conflict_cols
            .as_ref()
            .map(|cols| cols.iter().map(|c| c.to_lowercase()).collect())
            .unwrap_or_else(|| vec!["id".to_string()]);
        insert.on = Some(OnInsert::OnConflict(make_do_update(
            columns,
            conflict_target,
            &conflict_col_names,
        )));
        insert.or = None;
        insert.replace_into = false;
        // Add RETURNING id when conflict target contains "id" (matches C behavior)
        if should_add_returning_id(&conflict_cols) {
            insert.returning = Some(vec![SelectItem::UnnamedExpr(Expr::Identifier(Ident::new(
                "id",
            )))]);
        }
    } else if is_ignore {
        // INSERT OR IGNORE → ON CONFLICT DO NOTHING
        insert.on = Some(OnInsert::OnConflict(OnConflict {
            conflict_target,
            action: OnConflictAction::DoNothing,
        }));
        insert.or = None;
    }
}

/// Known Plex table conflict target columns (for ON CONFLICT).
/// Returns a list of column names that form the conflict target.
/// This matches the C translator's conflict_targets[] array, which uses
/// UNIQUE constraints rather than always using the PK.
fn get_conflict_columns(table_name: &str) -> Option<Vec<&'static str>> {
    match table_name.to_lowercase().as_str() {
        // Tables with simple id PRIMARY KEY
        "tags"
        | "taggings"
        | "metadata_items"
        | "media_items"
        | "media_parts"
        | "media_streams"
        | "settings"
        | "accounts"
        | "directories"
        | "library_sections"
        | "statistics_media"
        | "statistics_resources"
        | "devices"
        | "play_queue_items"
        | "play_queue_generators"
        | "play_queues"
        | "activities"
        | "locations"
        | "plugins"
        | "media_grabs"
        | "versioned_metadata_items"
        | "external_metadata_items"
        | "external_metadata_sources"
        | "metadata_item_views"
        | "metadata_item_accounts"
        | "metadata_item_clusterings"
        | "media_item_settings"
        | "media_provider_resources"
        | "media_subscriptions"
        | "metadata_relations"
        | "metadata_subscription_desired_items"
        | "sync_schema_versions"
        | "spellfix_metadata_titles"
        | "section_locations"
        | "hub_templates"
        | "blobs" => Some(vec!["id"]),

        // Tables with UNIQUE constraints (not PK)
        "statistics_bandwidth" => Some(vec!["account_id", "device_id", "timespan", "at", "lan"]),
        "metadata_item_settings" => Some(vec!["account_id", "guid"]),
        "locatables" => Some(vec!["location_id", "locatable_id", "locatable_type"]),
        "location_places" => Some(vec!["location_id", "guid"]),
        "media_stream_settings" => Some(vec!["media_stream_id", "account_id"]),
        "preferences" => Some(vec!["name"]),
        "schema_migrations" => Some(vec!["version"]),

        _ => None,
    }
}

/// Check if RETURNING id should be added for this table's upsert.
/// Matches C behavior: add RETURNING id when "id" appears anywhere in
/// the conflict target columns string (substring match, like the C code's
/// strcasestr check). This means tables with conflict targets containing "id"
/// as a substring (e.g. "account_id") also get RETURNING id.
fn should_add_returning_id(conflict_cols: &Option<Vec<&str>>) -> bool {
    if let Some(cols) = conflict_cols {
        // Check if any conflict column contains "id" as a substring
        // (matches C behavior: strcasestr(conflict_columns, "id"))
        let joined = cols.join(", ");
        joined.to_lowercase().contains("id")
    } else {
        false
    }
}

/// Build `ON CONFLICT (target_cols) DO UPDATE SET col1 = EXCLUDED.col1, col2 = EXCLUDED.col2, ...`
/// Excludes:
///   - the `id` column (always — it's the PK and shouldn't be updated)
///   - the conflict target columns (they define the conflict, can't be updated)
fn make_do_update(
    columns: Vec<Ident>,
    conflict_target: Option<ConflictTarget>,
    exclude_cols: &[String],
) -> OnConflict {
    let assignments: Vec<Assignment> = columns
        .iter()
        .filter(|col| {
            let col_lower = col.value.to_lowercase();
            // Always skip `id` column
            if col_lower == "id" {
                return false;
            }
            // Skip conflict target columns
            !exclude_cols.iter().any(|ex| ex.to_lowercase() == col_lower)
        })
        .map(|col| Assignment {
            target: AssignmentTarget::ColumnName(ObjectName(vec![ObjectNamePart::Identifier(
                col.clone(),
            )])),
            value: Expr::CompoundIdentifier(vec![Ident::new("EXCLUDED"), col.clone()]),
        })
        .collect();

    OnConflict {
        conflict_target,
        action: OnConflictAction::DoUpdate(DoUpdate {
            assignments,
            selection: None,
        }),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::translate;

    #[test]
    fn upsert_insert_or_replace() {
        let r = translate("INSERT OR REPLACE INTO settings(id, value) VALUES(?, ?)").unwrap();
        assert!(r.sql.to_uppercase().contains("ON CONFLICT"));
        assert!(r.sql.to_uppercase().contains("DO UPDATE"));
        assert!(!r.sql.to_uppercase().contains("OR REPLACE"));
    }

    #[test]
    fn upsert_insert_or_ignore() {
        let r = translate("INSERT OR IGNORE INTO tags(tag) VALUES(?)").unwrap();
        assert!(r.sql.to_uppercase().contains("ON CONFLICT"));
        assert!(r.sql.to_uppercase().contains("DO NOTHING"));
        assert!(!r.sql.to_uppercase().contains("OR IGNORE"));
    }

    #[test]
    fn upsert_replace_into() {
        let r = translate("REPLACE INTO settings(id, value) VALUES(?, ?)").unwrap();
        assert!(r.sql.to_uppercase().contains("ON CONFLICT"));
        assert!(r.sql.to_uppercase().contains("DO UPDATE"));
        assert!(!r.sql.to_uppercase().contains("REPLACE INTO"));
    }

    #[test]
    fn upsert_normal_insert_unchanged() {
        let r = translate("INSERT INTO t (a, b) VALUES (1, 2)").unwrap();
        assert!(!r.sql.to_uppercase().contains("ON CONFLICT"));
    }

    #[test]
    fn upsert_on_conflict_already_present_unchanged() {
        let r = translate(
            "INSERT INTO settings(id, value) VALUES(?, ?) ON CONFLICT(id) DO UPDATE SET value = excluded.value",
        )
        .unwrap();
        assert!(r.sql.to_uppercase().contains("ON CONFLICT"));
        let count = r.sql.to_uppercase().matches("ON CONFLICT").count();
        assert_eq!(count, 1, "ON CONFLICT should appear exactly once");
    }
}
