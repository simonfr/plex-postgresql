use sqlparser::ast::Statement;

use crate::{groupby, query};

use super::rules::{RewriteContext, RewriteRule};

struct GroupByRule;
struct QueryRule;

impl RewriteRule for GroupByRule {
    fn name(&self) -> &'static str {
        "groupby"
    }

    fn apply(&self, stmt: &mut Statement, _ctx: &mut RewriteContext<'_>) {
        groupby::transform(stmt);
    }
}

impl RewriteRule for QueryRule {
    fn name(&self) -> &'static str {
        "query"
    }

    fn apply(&self, stmt: &mut Statement, _ctx: &mut RewriteContext<'_>) {
        query::transform(stmt);
    }
}

pub fn plex_rules() -> Vec<Box<dyn RewriteRule>> {
    vec![Box::new(GroupByRule), Box::new(QueryRule)]
}
