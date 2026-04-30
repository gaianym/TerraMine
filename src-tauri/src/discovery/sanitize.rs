use serde_json::Value;

const MAX_DEPTH: usize = 24;
const MAX_ARRAY_LEN: usize = 256;
const MAX_STRING_BYTES: usize = 4096;
const MAX_OBJECT_KEYS: usize = 128;

pub fn sanitize_error_message(msg: &str) -> String {
    let mut s = msg
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(512)
        .collect::<String>();
    if s.len() > 500 {
        s.truncate(500);
        s.push('…');
    }
    s
}

pub fn sanitize_miner_json(mut v: Value) -> Value {
    sanitize_value(&mut v, 0);
    v
}

fn sanitize_value(v: &mut Value, depth: usize) {
    if depth > MAX_DEPTH {
        *v = Value::String("(truncated)".to_string());
        return;
    }
    match v {
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
        Value::String(s) => {
            if s.len() > MAX_STRING_BYTES {
                s.truncate(MAX_STRING_BYTES);
                s.push_str("…");
            }
        }
        Value::Array(arr) => {
            if arr.len() > MAX_ARRAY_LEN {
                arr.truncate(MAX_ARRAY_LEN);
                arr.push(Value::String("…".to_string()));
            }
            for item in arr.iter_mut() {
                sanitize_value(item, depth + 1);
            }
        }
        Value::Object(map) => {
            if map.len() > MAX_OBJECT_KEYS {
                let keys: Vec<_> = map.keys().cloned().collect();
                map.retain(|k, _| keys.iter().take(MAX_OBJECT_KEYS).any(|x| x == k));
                map.insert(
                    "_truncated".to_string(),
                    Value::String("true".to_string()),
                );
            }
            for (_, val) in map.iter_mut() {
                sanitize_value(val, depth + 1);
            }
        }
    }
}
