use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Eq, PartialEq, Copy, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum BookingStatus {
    Pending,
    Confirmed,
    Failed,
}

impl BookingStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            BookingStatus::Pending => "PENDING",
            BookingStatus::Confirmed => "CONFIRMED",
            BookingStatus::Failed => "FAILED",
        }
    }
}

impl std::str::FromStr for BookingStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "PENDING" => Ok(BookingStatus::Pending),
            "CONFIRMED" => Ok(BookingStatus::Confirmed),
            "FAILED" => Ok(BookingStatus::Failed),
            other => Err(format!("Unkown booking status: {other:?}")),
        }
    }
}

/// POST /bookings request body
#[derive(Serialize, Deserialize)]
pub struct CreateBookingRequest {
    pub show_id: i64,
}

/// POST /bookings response body
#[derive(Serialize, Deserialize)]
pub struct BookingResponse {
    pub id: String,
    pub show_id: i64,
    pub status: BookingStatus,
}
