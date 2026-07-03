use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;
use validator::Validate;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "kyc_status", rename_all = "snake_case")]
pub enum KycStatus {
    Pending,
    Verified,
    Rejected,
    UnderReview,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "address_type", rename_all = "snake_case")]
pub enum AddressType {
    Residential,
    Mailing,
    Business,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "document_type", rename_all = "snake_case")]
pub enum DocumentType {
    Passport,
    DriversLicense,
    HealthCard,
    UtilityBill,
    BankStatement,
    EmploymentLetter,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "verification_status", rename_all = "snake_case")]
pub enum VerificationStatus {
    Pending,
    Verified,
    Rejected,
    Expired,
}

// Core Customer Entity
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Customer {
    pub customer_id: Uuid,
    pub email: String,
    pub phone_number: String,
    pub first_name: String,
    pub last_name: String,
    pub date_of_birth: NaiveDate,
    pub sin: Option<String>,
    pub kyc_status: KycStatus,
    pub kyc_completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// Customer creation request
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct CreateCustomerRequest {
    #[validate(email)]
    pub email: String,

    #[validate(length(min = 10, max = 20))]
    pub phone_number: String,

    #[validate(length(min = 1, max = 100))]
    pub first_name: String,

    #[validate(length(min = 1, max = 100))]
    pub last_name: String,

    pub date_of_birth: NaiveDate,

    #[validate(length(equal = 9))]
    pub sin: Option<String>,

    #[validate(length(min = 8))]
    pub password: String,
}

// Customer update request
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct UpdateCustomerRequest {
    #[validate(length(min = 10, max = 20))]
    pub phone_number: Option<String>,

    #[validate(length(min = 1, max = 100))]
    pub first_name: Option<String>,

    #[validate(length(min = 1, max = 100))]
    pub last_name: Option<String>,
}

// Customer Address Entity
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct CustomerAddress {
    pub address_id: Uuid,
    pub customer_id: Uuid,
    pub address_type: AddressType,
    pub street_address: String,
    pub city: String,
    pub province: String,
    pub postal_code: String,
    pub country: String,
    pub is_primary: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// Address creation request
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct CreateAddressRequest {
    pub address_type: AddressType,

    #[validate(length(min = 1, max = 255))]
    pub street_address: String,

    #[validate(length(min = 1, max = 100))]
    pub city: String,

    #[validate(length(equal = 2))]
    pub province: String,

    #[validate(length(equal = 7))]
    pub postal_code: String,

    pub is_primary: bool,
}

// KYC Document Entity
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct KycDocument {
    pub document_id: Uuid,
    pub customer_id: Uuid,
    pub document_type: DocumentType,
    pub file_path: String,
    pub file_name: String,
    pub verification_status: VerificationStatus,
    pub verified_by: Option<String>,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub verified_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
}

// KYC Document upload request
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct UploadKycDocumentRequest {
    pub document_type: DocumentType,

    #[validate(length(min = 1, max = 255))]
    pub file_name: String,
    // File content would be handled separately (multipart upload)
}

// Customer response (without sensitive data)
#[derive(Debug, Serialize, Deserialize)]
pub struct CustomerResponse {
    pub customer_id: Uuid,
    pub email: String,
    pub phone_number: String,
    pub first_name: String,
    pub last_name: String,
    pub kyc_status: KycStatus,
    pub kyc_completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl From<Customer> for CustomerResponse {
    fn from(customer: Customer) -> Self {
        Self {
            customer_id: customer.customer_id,
            email: customer.email,
            phone_number: customer.phone_number,
            first_name: customer.first_name,
            last_name: customer.last_name,
            kyc_status: customer.kyc_status,
            kyc_completed_at: customer.kyc_completed_at,
            created_at: customer.created_at,
        }
    }
}
