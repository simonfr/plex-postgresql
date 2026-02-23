use sqlparser::ast::*;

/// Remove duplicate column assignments in UPDATE statements, keeping only the
/// last assignment for each column.  PostgreSQL rejects duplicate target columns
/// whereas SQLite silently accepts them (last-writer-wins).
pub fn transform(stmt: &mut Statement) {
    if let Statement::Update(update) = stmt {
        dedup_assignments(&mut update.assignments);
    }
}

fn dedup_assignments(assignments: &mut Vec<Assignment>) {
    // Walk backwards so the *last* assignment for a given column is the one we
    // keep (it is encountered first during the reverse scan).
    let mut seen = std::collections::HashSet::new();
    let mut i = assignments.len();
    while i > 0 {
        i -= 1;
        let key = assignment_column_key(&assignments[i]);
        if !seen.insert(key) {
            assignments.remove(i);
        }
    }
}

/// Produce a lowercase, dot-joined string key for the target of an assignment
/// so that `a`, `"a"`, and `` `a` `` all compare equal.
fn assignment_column_key(assignment: &Assignment) -> String {
    match &assignment.target {
        AssignmentTarget::ColumnName(name) => name
            .0
            .iter()
            .map(|part| match part {
                ObjectNamePart::Identifier(ident) => ident.value.to_lowercase(),
                other => other.to_string().to_lowercase(),
            })
            .collect::<Vec<_>>()
            .join("."),
        // Tuple targets (a, b) = (1, 2) — stringify as-is for dedup key
        AssignmentTarget::Tuple(cols) => cols
            .iter()
            .map(|c| c.to_string().to_lowercase())
            .collect::<Vec<_>>()
            .join(","),
    }
}

#[cfg(test)]
mod tests {
    use crate::translate;

    #[test]
    fn update_duplicate_set_column_keeps_last_and_renumbers_placeholders() {
        let r = translate("UPDATE directories SET `updated_at`=:U1,`updated_at`=:U2 WHERE `id`=:C1")
            .unwrap();
        let sql = r.sql.to_lowercase();

        assert!(
            sql.contains("set \"updated_at\" = $1"),
            "Expected deduped SET to use $1, got: {}",
            r.sql
        );
        assert!(
            sql.contains("where \"id\" = $2"),
            "Expected WHERE placeholder to be renumbered to $2, got: {}",
            r.sql
        );
        assert!(!sql.contains("$3"), "Unexpected placeholder gap, got: {}", r.sql);
        assert_eq!(
            r.param_names,
            vec![Some("U2".to_string()), Some("C1".to_string())]
        );
    }
}
