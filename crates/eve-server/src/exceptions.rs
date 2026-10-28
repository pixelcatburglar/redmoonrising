//! Exceptions the client can unmarshal, sent back inside a wrapped-exception error.

use eve_marshal::Value;

/// `ccp_exceptions.UserError(msg, dict)`: a refusal the client shows as a message box.
pub fn user_error(msg: &str, args: &[(&str, Value)]) -> Value {
    let msg_v = Value::bytes(msg);
    let (dict, exc_args) = if args.is_empty() {
        (Value::None, Value::Tuple(vec![msg_v.clone()]))
    } else {
        let d = Value::Dict(args.iter().map(|(k, v)| (Value::bytes(*k), v.clone())).collect());
        (d.clone(), Value::Tuple(vec![msg_v.clone(), d]))
    };
    Value::Instance {
        class: Box::new(Value::bytes("ccp_exceptions.UserError")),
        state: Box::new(Value::Dict(vec![
            (Value::bytes("msg"), msg_v),
            (Value::bytes("dict"), dict),
            (Value::bytes("args"), exc_args),
        ])),
    }
}

/// A `CustomError` user error carrying free text.
pub fn custom_error(text: &str) -> Value {
    user_error("CustomError", &[("error", Value::bytes(text))])
}

/// `exceptions.ServiceNotFound`: what the server answers for a call it does not implement.
pub fn service_not_found(what: &str) -> Value {
    let name = Value::bytes(what);
    Value::Instance {
        class: Box::new(Value::bytes("exceptions.ServiceNotFound")),
        state: Box::new(Value::Dict(vec![
            (Value::bytes("serviceName"), name.clone()),
            (Value::bytes("args"), Value::List(vec![name])),
        ])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class_and_state(v: &Value) -> (String, &Value) {
        let Value::Instance { class, state } = v else { panic!("not an instance: {v:?}") };
        (String::from_utf8_lossy(class.as_bytes().unwrap()).into_owned(), &**state)
    }

    #[test]
    fn a_user_error_is_ccp_exceptions_with_its_dict_as_state() {
        let err = user_error("NotEnoughMoney", &[("amount", Value::Int(5))]);
        let (class, state) = class_and_state(&err);
        assert_eq!(class, "ccp_exceptions.UserError");
        assert_eq!(state.get_str("msg"), Some(&Value::bytes("NotEnoughMoney")));
        let dict = Value::Dict(vec![(Value::bytes("amount"), Value::Int(5))]);
        assert_eq!(state.get_str("dict"), Some(&dict));
        assert_eq!(state.get_str("args"), Some(&Value::Tuple(vec![Value::bytes("NotEnoughMoney"), dict])));
    }

    #[test]
    fn a_user_error_without_arguments_mirrors_the_one_argument_constructor() {
        let err = user_error("ShipNotYours", &[]);
        let (_, state) = class_and_state(&err);
        assert_eq!(state.get_str("dict"), Some(&Value::None));
        assert_eq!(state.get_str("args"), Some(&Value::Tuple(vec![Value::bytes("ShipNotYours")])));
    }

    #[test]
    fn an_unimplemented_method_is_a_real_service_not_found() {
        let err = service_not_found("character::GetHomeStation");
        let (class, state) = class_and_state(&err);
        assert_eq!(class, "exceptions.ServiceNotFound");
        let name = Value::bytes("character::GetHomeStation");
        assert_eq!(state.get_str("serviceName"), Some(&name));
        assert_eq!(state.get_str("args"), Some(&Value::List(vec![name])));
    }
}
