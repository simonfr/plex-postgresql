use std::os::raw::c_char;

#[repr(C)]
pub struct SqlTranslation {
    pub sql: *mut c_char,
    pub param_names: *mut *mut c_char,
    pub param_count: i32,
    pub success: i32,
    pub error: [u8; 256],
}

unsafe extern "C" {
    pub fn sql_translate(sqlite_sql: *const c_char) -> SqlTranslation;
    pub fn sql_translation_free(result: *mut SqlTranslation);

    pub fn sql_translator_init();
    pub fn sql_translator_cleanup();

    pub fn sql_translator_translate(sql: *const c_char) -> *mut c_char;
    pub fn sql_translator_free(ptr: *mut c_char);
    pub fn sql_translator_last_error() -> *const c_char;

    pub fn sql_translator_translate_full(sql: *const c_char) -> SqlTranslation;
    pub fn sql_translator_translation_free(t: *mut SqlTranslation);
}
