//! `languageSvc`. Only English exists: the client's own strings are English, so
//! the language packs are empty and mls.TR falls back to its default text.

use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::rowset;

pub async fn language_svc(call: &Call, _ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        // gameui.GetLanguages: [languageID, languageName, translatedLanguageName].
        "GetLanguages" => Value::List(vec![Value::List(vec![
            Value::bytes("EN"),
            Value::bytes("English"),
            Value::bytes("English"),
        ])]),
        // mlsUtil.LoadLanguagePack: {(columnID, keyID): text}.
        "GetMessagesByLanguageID" => Value::Dict(Vec::new()),
        // slides.__LoadTextGroup indexes the rows by textLabel.
        "GetTextsForGroup" => rowset(&["textLabel", "text"], Vec::new()),
        _ => return None,
    };
    Some(Outcome::Value(value))
}
