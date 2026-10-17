//! Chat core: session state and the message -> connector calls -> reply loop.
//! Knows the connector trait and domain types, never a specific platform or UI.

pub use connectors::Credentials;
use connectors::Platform;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

/// Everything the app knows about the current conversation. No login: the
/// shop URL identifies the session.
#[derive(Debug, Default)]
pub struct Session {
    pub shop_url: String,
    pub shop: Option<ShopConnection>,
    pub history: Vec<Message>,
}

#[derive(Debug, Clone)]
pub struct ShopConnection {
    pub platform: Platform,
    pub credentials: Credentials,
}

impl Session {
    pub fn new(shop_url: impl Into<String>) -> Self {
        Self {
            shop_url: shop_url.into(),
            ..Default::default()
        }
    }

    pub fn connect_shop(&mut self, platform: Platform, credentials: Credentials) {
        self.shop = Some(ShopConnection {
            platform,
            credentials,
        });
    }

    /// Appends the user message and the assistant reply to the history.
    /// Intent detection and connector dispatch are not implemented yet.
    pub fn handle_message(&mut self, text: impl Into<String>) -> &Message {
        let text = text.into();
        self.history.push(Message {
            role: Role::User,
            text: text.clone(),
        });
        let reply = match &self.shop {
            None => {
                "No shop connected yet. Use the + button to add the shop connector.".to_string()
            }
            Some(shop) => format!(
                "Connected to {} at {}. I can't act on \"{}\" yet.",
                shop.platform.display_name(),
                self.shop_url,
                text
            ),
        };
        self.history.push(Message {
            role: Role::Assistant,
            text: reply,
        });
        self.history.last().expect("reply was just pushed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_without_shop_points_to_connector_menu() {
        let mut s = Session::new("https://shop.example");
        let reply = s.handle_message("list orders");
        assert_eq!(reply.role, Role::Assistant);
        assert!(reply.text.contains("+ button"));
        assert_eq!(s.history.len(), 2);
    }

    #[test]
    fn reply_with_shop_names_platform_and_url() {
        let mut s = Session::new("https://shop.example");
        s.connect_shop(Platform::Shopify, Credentials::new());
        let reply = s.handle_message("list orders").text.clone();
        assert!(reply.contains("Shopify"));
        assert!(reply.contains("https://shop.example"));
    }
}
