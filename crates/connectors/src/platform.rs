use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Platform {
    WooCommerce,
    Magento,
    PrestaShop,
    Shopify,
}

impl Platform {
    pub const ALL: [Platform; 4] = [
        Platform::WooCommerce,
        Platform::Magento,
        Platform::PrestaShop,
        Platform::Shopify,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Platform::WooCommerce => "woocommerce",
            Platform::Magento => "magento",
            Platform::PrestaShop => "prestashop",
            Platform::Shopify => "shopify",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.id() == id)
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Platform::WooCommerce => "WooCommerce",
            Platform::Magento => "Magento",
            Platform::PrestaShop => "PrestaShop",
            Platform::Shopify => "Shopify",
        }
    }
}
