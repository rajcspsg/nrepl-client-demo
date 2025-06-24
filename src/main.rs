use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct NreplClient {
    stream: TcpStream,
    session: Option<String>,
}

pub struct EvalResult {
    pub value: Option<String>,
    pub output: String,
    pub error: String,
    pub has_error: bool,
}

impl Default for EvalResult {
    fn default() -> Self {
        EvalResult {
            value: None,
            output: String::new(),
            error: String::new(),
            has_error: false,
        }
    }
}

impl NreplClient {
    pub fn connect(host: &str, port: u16) -> Result<Self, Box<dyn std::error::Error>> {
        let mut stream = TcpStream::connect(format!("{}:{}", host, port))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;

        Ok(NreplClient {
            stream,
            session: None,
        })
    }

    pub fn clone_session(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        let mut msg = HashMap::new();
        msg.insert(
            "op".to_string(),
            serde_bencode::value::Value::Bytes(b"clone".to_vec()),
        );
        msg.insert(
            "id".to_string(),
            serde_bencode::value::Value::Bytes(uuid::Uuid::new_v4().to_string().into_bytes()),
        );

        self.send_message(&msg)?;
        let response = self.read_message()?;

        if let Some(new_session) = response.get("new-session") {
            if let serde_bencode::value::Value::Bytes(session_bytes) = new_session {
                let session_id = String::from_utf8_lossy(session_bytes).to_string();
                self.session = Some(session_id.clone());
                return Ok(session_id);
            }
        }

        Err("Failed to get session from clone response".into())
    }

    pub fn eval(&mut self, code: &str) -> Result<EvalResult, Box<dyn std::error::Error>> {
        // Ensure we have a session
        if self.session.is_none() {
            self.clone_session()?;
        }

        let mut msg = HashMap::new();
        msg.insert(
            "op".to_string(),
            serde_bencode::value::Value::Bytes(b"eval".to_vec()),
        );
        msg.insert(
            "id".to_string(),
            serde_bencode::value::Value::Bytes(uuid::Uuid::new_v4().to_string().into_bytes()),
        );
        msg.insert(
            "code".to_string(),
            serde_bencode::value::Value::Bytes(code.as_bytes().to_vec()),
        );

        if let Some(session) = &self.session {
            msg.insert(
                "session".to_string(),
                serde_bencode::value::Value::Bytes(session.as_bytes().to_vec()),
            );
        }

        self.send_message(&msg)?;

        let mut result = EvalResult::default();

        // Keep reading responses until we get "done" status
        loop {
            let response = self.read_message()?;

            // Extract value
            if let Some(serde_bencode::value::Value::Bytes(value_bytes)) = response.get("value") {
                result.value = Some(String::from_utf8_lossy(value_bytes).to_string());
            }

            // Extract stdout
            if let Some(serde_bencode::value::Value::Bytes(out_bytes)) = response.get("out") {
                result.output.push_str(&String::from_utf8_lossy(out_bytes));
            }

            // Extract stderr
            if let Some(serde_bencode::value::Value::Bytes(err_bytes)) = response.get("err") {
                result.error.push_str(&String::from_utf8_lossy(err_bytes));
            }

            // Check status
            if let Some(serde_bencode::value::Value::List(status_list)) = response.get("status") {
                let mut is_done = false;
                for status_item in status_list {
                    if let serde_bencode::value::Value::Bytes(status_bytes) = status_item {
                        let status_str = String::from_utf8_lossy(status_bytes);
                        match status_str.as_ref() {
                            "done" => is_done = true,
                            "error" => result.has_error = true,
                            _ => {}
                        }
                    }
                }
                if is_done {
                    break;
                }
            }
        }

        Ok(result)
    }

    pub fn describe(
        &mut self,
    ) -> Result<HashMap<String, serde_bencode::value::Value>, Box<dyn std::error::Error>> {
        let mut msg = HashMap::new();
        msg.insert(
            "op".to_string(),
            serde_bencode::value::Value::Bytes(b"describe".to_vec()),
        );
        msg.insert(
            "id".to_string(),
            serde_bencode::value::Value::Bytes(uuid::Uuid::new_v4().to_string().into_bytes()),
        );

        self.send_message(&msg)?;
        self.read_message()
    }

    fn send_message(
        &mut self,
        msg: &HashMap<String, serde_bencode::value::Value>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let encoded = serde_bencode::to_bytes(msg)?;
        println!("Sending: {}", String::from_utf8_lossy(&encoded)); // Debug
        self.stream.write_all(&encoded)?;
        self.stream.flush()?;
        Ok(())
    }

    fn read_message(
        &mut self,
    ) -> Result<HashMap<String, serde_bencode::value::Value>, Box<dyn std::error::Error>> {
        // Read the entire stream content until we can decode a complete bencode message
        let mut buffer = Vec::new();
        let mut temp_buffer = [0u8; 4096];

        // Try to read data
        match self.stream.read(&mut temp_buffer) {
            Ok(0) => return Err("Connection closed".into()),
            Ok(n) => {
                buffer.extend_from_slice(&temp_buffer[..n]);
            }
            Err(e) => return Err(e.into()),
        }

        // Try to decode what we have
        let mut pos = 0;
        while pos < buffer.len() {
            match serde_bencode::from_bytes::<HashMap<String, serde_bencode::value::Value>>(
                &buffer[pos..],
            ) {
                Ok(decoded) => {
                    println!("Received: {:?}", decoded); // Debug
                    return Ok(decoded);
                }
                Err(_) => {
                    // If we can't decode, try reading more data
                    let mut additional = [0u8; 1024];
                    match self.stream.read(&mut additional) {
                        Ok(0) => break, // No more data
                        Ok(n) => {
                            buffer.extend_from_slice(&additional[..n]);
                        }
                        Err(_) => break, // Read error
                    }
                }
            }
        }

        // If we still can't decode, try a different approach
        // Sometimes multiple messages are concatenated
        for i in 1..buffer.len() {
            if let Ok(decoded) = serde_bencode::from_bytes::<
                HashMap<String, serde_bencode::value::Value>,
            >(&buffer[..i])
            {
                println!("Received (partial): {:?}", decoded); // Debug
                return Ok(decoded);
            }
        }

        Err(format!("Failed to decode message from {} bytes", buffer.len()).into())
    }

    pub fn close(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(session) = &self.session.clone() {
            let mut msg = HashMap::new();
            msg.insert(
                "op".to_string(),
                serde_bencode::value::Value::Bytes(b"close".to_vec()),
            );
            msg.insert(
                "id".to_string(),
                serde_bencode::value::Value::Bytes(uuid::Uuid::new_v4().to_string().into_bytes()),
            );
            msg.insert(
                "session".to_string(),
                serde_bencode::value::Value::Bytes(session.as_bytes().to_vec()),
            );

            let _ = self.send_message(&msg); // Ignore errors on close
            self.session = None;
        }
        Ok(())
    }
}

impl Drop for NreplClient {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

// Simpler test client that just tries basic operations
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Connecting to nREPL server...");
    let mut client = NreplClient::connect("127.0.0.1", 55821)?;
    println!("Connected!");

    // Test describe first
    println!("\n=== Testing describe ===");
    match client.describe() {
        Ok(desc) => {
            println!("Server description successful");
            if let Some(ops) = desc.get("ops") {
                println!("Available operations: {:?}", ops);
            }
        }
        Err(e) => println!("Describe failed: {}", e),
    }

    // Test session creation
    println!("\n=== Testing session creation ===");
    match client.clone_session() {
        Ok(session) => println!("Session created: {}", session),
        Err(e) => println!("Session creation failed: {}", e),
    }

    // Test simple evaluation
    println!("\n=== Testing evaluation ===");
    match client.eval("(+ 1 2 3)") {
        Ok(result) => {
            println!("Evaluation successful!");
            println!("Value: {:?}", result.value);
            println!("Output: {}", result.output);
            if result.has_error {
                println!("Error: {}", result.error);
            }
        }
        Err(e) => println!("Evaluation failed: {}", e),
    }

    // Test output
    println!("\n=== Testing output ===");
    match client.eval("(println \"Hello from Rust!\")") {
        Ok(result) => {
            println!("Output test successful!");
            println!("Value: {:?}", result.value);
            println!("Output: '{}'", result.output);
        }
        Err(e) => println!("Output test failed: {}", e),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_connection() {
        // Start nREPL server with: lein repl :headless :host 127.0.0.1 :port 7888
        if let Ok(mut client) = NreplClient::connect("127.0.0.1", 7888) {
            // Test describe
            assert!(client.describe().is_ok());

            // Test session creation
            assert!(client.clone_session().is_ok());

            // Test simple eval
            let result = client.eval("(+ 1 1)").unwrap();
            assert_eq!(result.value, Some("2".to_string()));
        }
    }
}
