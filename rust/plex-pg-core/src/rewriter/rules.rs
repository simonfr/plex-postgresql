use sqlparser::ast::Statement;

use super::{core_rules, plex_rules};

pub struct RewriteContext<'a> {
    pub param_names: &'a mut Vec<Option<String>>,
}

pub trait RewriteRule {
    fn name(&self) -> &'static str;
    fn apply(&self, stmt: &mut Statement, ctx: &mut RewriteContext<'_>);
}

pub fn default_rules() -> Vec<Box<dyn RewriteRule>> {
    let mut rules = core_rules::core_rules();
    rules.extend(plex_rules::plex_rules());
    rules
}
