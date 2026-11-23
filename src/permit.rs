use crate::{AppSession, Error, Result, secret, text, transport};
use reqwest::{
    Client,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use transport::{APP, PERMIT};

/// IDs are sent with the scalar type provided by the upstream response.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Id {
    Number(u64),
    Text(String),
}

impl Id {
    fn validate(&self) -> Result<()> {
        if let Self::Text(value) = self {
            text(value, "id", 256)?;
        }
        Ok(())
    }

    fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Number(n), Self::Text(s)) | (Self::Text(s), Self::Number(n)) => {
                n.to_string() == *s
            }
            _ => self == other,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum PermitKind {
    InsideSixthRing,
    OutsideSixthRing,
}

impl PermitKind {
    pub const fn code(self) -> &'static str {
        match self {
            Self::InsideSixthRing => "01",
            Self::OutsideSixthRing => "02",
        }
    }
}

/// Vehicle linked to the current official account.
#[derive(Deserialize)]
pub struct Vehicle {
    #[serde(rename = "vId")]
    pub id: Id,
    #[serde(rename = "hphm")]
    pub plate: String,
    #[serde(rename = "hpzl")]
    pub plate_type: String,
    #[serde(rename = "cllx", default)]
    pub vehicle_type: Option<String>,
}

/// Fields from the vehicle registration document and the official form.
#[derive(Serialize)]
pub struct VehicleRegistration<'a> {
    #[serde(rename = "hphm")]
    pub plate: &'a str,
    #[serde(rename = "hpzl")]
    pub plate_type: &'a str,
    #[serde(rename = "hpzlmc")]
    pub plate_type_name: &'a str,
    #[serde(rename = "cllx")]
    pub vehicle_type: &'a str,
    #[serde(rename = "cllxmc")]
    pub vehicle_type_name: &'a str,
    #[serde(rename = "fdjh")]
    pub engine_suffix: &'a str,
    #[serde(rename = "ppxh")]
    pub model: &'a str,
    #[serde(rename = "zcsj")]
    pub registration_date: &'a str,
    #[serde(rename = "kz3")]
    pub truck_subtype: &'a str,
}

/// Official final form data, validated locally before it can be sent.
/// Construct only after showing the applicant the data for confirmation.
pub struct Application(Value);

const APPLICATION_FIELDS: &[&str] = &[
    "vId",
    "hphm",
    "hpzl",
    "ylzsfkb",
    "elzsfkb",
    "elzqyms",
    "ylzqyms",
    "elzmc",
    "ylzmc",
    "cllx",
    "jjzzl",
    "jsrxm",
    "jszh",
    "dabh",
    "rylx",
    "txrxx",
    "jjrq",
    "jjmd",
    "jjmdmc",
    "area",
    "jjdq",
    "xxdz",
    "jjdzgdjd",
    "jjdzgdwd",
    "sqdzgdjd",
    "sqdzgdwd",
    "jingState",
    "sfzj",
    "jjlk",
    "jjlkmc",
    "jjlkgdjd",
    "jjlkgdwd",
    "zjxxdz",
    "zjxxdzgdjd",
    "zjxxdzgdwd",
    "xslx",
];

impl Application {
    /// Assemble a reviewed application from an official `states()["bzclxx"]` vehicle
    /// row, checked driver fields and the applicant's confirmed trip fields.
    pub fn from_parts(
        vehicle_state: &Value,
        kind: PermitKind,
        driver: &Value,
        trip: Value,
    ) -> Result<Self> {
        let mut fields = trip.as_object().ok_or(Error::InvalidInput("trip"))?.clone();
        for (source, keys) in [
            (
                vehicle_state,
                &[
                    "vId", "hphm", "hpzl", "cllx", "ylzsfkb", "elzsfkb", "ylzqyms", "elzqyms",
                    "ylzmc", "elzmc",
                ][..],
            ),
            (driver, &["jsrxm", "jszh", "dabh", "rylx", "txrxx"][..]),
        ] {
            for &key in keys {
                if fields.contains_key(key) {
                    return Err(Error::InvalidInput("trip_fields"));
                }
                if let Some(value) = source.get(key) {
                    fields.insert(key.into(), value.clone());
                }
            }
        }
        if fields.insert("jjzzl".into(), json!(kind.code())).is_some() {
            return Err(Error::InvalidInput("trip_fields"));
        }
        Self::from_official_fields(Value::Object(fields))
    }

    /// Build from reviewed H5 fields. Preserve the original types of upstream-supplied values.
    pub fn from_official_fields(fields: Value) -> Result<Self> {
        let object = fields
            .as_object()
            .ok_or(Error::InvalidInput("application"))?;
        if object
            .keys()
            .any(|key| !APPLICATION_FIELDS.contains(&key.as_str()))
        {
            return Err(Error::InvalidInput("application_fields"));
        }
        let id: Id = serde_json::from_value(fields["vId"].clone())
            .map_err(|_| Error::InvalidInput("vId"))?;
        id.validate()?;
        for field in [
            "hphm", "hpzl", "cllx", "jsrxm", "jszh", "jjrq", "jjmd", "jjmdmc", "area", "jjdq",
            "xxdz",
        ] {
            text(
                fields[field].as_str().ok_or(Error::InvalidInput(field))?,
                field,
                256,
            )?;
        }
        if !matches!(fields["jjzzl"].as_str(), Some("01" | "02")) {
            return Err(Error::InvalidInput("jjzzl"));
        }
        if let Some(kind) = fields.get("rylx")
            && !matches!(kind.as_str(), Some("0" | "1"))
        {
            return Err(Error::InvalidInput("rylx"));
        }
        validate_passengers(&fields["txrxx"])?;
        let (required, excluded) = match fields["sfzj"].as_str() {
            Some("0") => ("jjlkmc", &["zjxxdz", "zjxxdzgdjd", "zjxxdzgdwd"][..]),
            Some("1") => ("zjxxdz", &["jjlk", "jjlkmc", "jjlkgdjd", "jjlkgdwd"][..]),
            _ => return Err(Error::InvalidInput("sfzj")),
        };
        text(
            fields[required]
                .as_str()
                .ok_or(Error::InvalidInput(required))?,
            required,
            256,
        )?;
        if excluded
            .iter()
            .any(|key| !matches!(&fields[*key], Value::Null) && fields[*key] != "")
        {
            return Err(Error::InvalidInput("entry_location"));
        }
        if fields["sfzj"] == "1" {
            location_pair(&fields, "zjxxdzgdjd", "zjxxdzgdwd", true)?;
        } else if fields["jjlk"].as_str().is_none_or(str::is_empty) {
            location_pair(&fields, "jjlkgdjd", "jjlkgdwd", true)?;
        } else {
            location_pair(&fields, "jjlkgdjd", "jjlkgdwd", false)?;
        }
        location_pair(&fields, "jjdzgdjd", "jjdzgdwd", false)?;
        location_pair(&fields, "sqdzgdjd", "sqdzgdwd", false)?;
        if let Some(route) = fields.get("xslx")
            && route != ""
        {
            let places: Vec<String> =
                serde_json::from_str(route.as_str().ok_or(Error::InvalidInput("xslx"))?)
                    .map_err(|_| Error::InvalidInput("xslx"))?;
            if places.len() > 20 {
                return Err(Error::InvalidInput("xslx"));
            }
            for place in places {
                text(&place, "xslx", 256)?;
            }
        }
        Ok(Self(fields))
    }
}

fn location_pair(
    fields: &Value,
    lon: &'static str,
    lat: &'static str,
    required: bool,
) -> Result<()> {
    if !required
        && [lon, lat]
            .iter()
            .all(|key| fields[*key].is_null() || fields[*key] == "")
    {
        return Ok(());
    }
    for (key, limit) in [(lon, 180.0), (lat, 90.0)] {
        let value = fields[key]
            .as_f64()
            .or_else(|| fields[key].as_str()?.parse().ok());
        if !value.is_some_and(|n| (-limit..=limit).contains(&n)) {
            return Err(Error::InvalidInput(key));
        }
    }
    Ok(())
}

fn validate_passengers(value: &Value) -> Result<()> {
    let rows = value.as_array().ok_or(Error::InvalidInput("txrxx"))?;
    if rows.len() > 10
        || rows.iter().any(|row| {
            !row.as_object().is_some_and(|object| {
                object.iter().all(|(key, value)| {
                    matches!(key.as_str(), "txrxm" | "txrsfzh" | "txrsjh")
                        && value.as_str().is_some_and(|s| {
                            s.chars().count() <= 128 && !s.chars().any(char::is_control)
                        })
                })
            })
        })
    {
        return Err(Error::InvalidInput("txrxx"));
    }
    Ok(())
}

/// One official account's permit session. Use separate clients for separate accounts.
pub struct PermitClient {
    http: Client,
    app: AppSession,
    app_origin: String,
    permit_origin: String,
    state: String,
    source: HeaderValue,
    authorization: Option<HeaderValue>,
}

impl PermitClient {
    /// Supply the business state and source assigned to your authorized integration.
    pub fn new(app: AppSession, state: &str, source: &str) -> Result<Self> {
        text(state, "permit_state", 128)?;
        Ok(Self {
            http: transport::client(false)?,
            app,
            app_origin: APP.to_owned(),
            permit_origin: PERMIT.to_owned(),
            state: state.to_owned(),
            source: secret(source, "permit_source")?,
            authorization: None,
        })
    }

    async fn authorize(&self) -> Result<HeaderValue> {
        let sso = transport::json(
            self.http
                .get(format!("{}/auth/getSsoUserToken", self.app_origin))
                .header(AUTHORIZATION, self.app.token.clone()),
        )
        .await?;
        let sso = transport::business(sso, "sso_token")?;
        let token = sso["data"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or(Error::InvalidResponse)?;
        let reply = transport::json(
            self.http
                .post(format!(
                    "{}/auth/userController/loginUserByUserCenter",
                    self.permit_origin
                ))
                .query(&[("state", &self.state)])
                .header("source", self.source.clone())
                .header("origin", &self.permit_origin)
                .json(&json!({"token": token, "state": self.state})),
        )
        .await?;
        let reply = transport::business(reply, "permit_login")?;
        secret(
            reply["data"]["accessToken"]
                .as_str()
                .ok_or(Error::InvalidResponse)?,
            "business_token",
        )
        .map_err(|_| Error::InvalidResponse)
    }

    async fn request(&mut self, endpoint: &'static str, data: Value, write: bool) -> Result<Value> {
        let body = serde_json::to_vec(&data).map_err(|_| Error::InvalidInput("request_body"))?;
        if body.len() > 65_536 {
            return Err(Error::InvalidInput("request_body"));
        }
        if self.authorization.is_none() {
            self.authorization = Some(self.authorize().await?);
        }
        let authorization = self
            .authorization
            .as_ref()
            .ok_or(Error::Unauthorized)?
            .clone();
        let result = transport::json(
            self.http
                .post(format!("{}/pro/{endpoint}", self.permit_origin))
                .header(AUTHORIZATION, authorization)
                .header("origin", &self.permit_origin)
                .header("accept", "application/json")
                .header("content-type", "application/json;charset=UTF-8")
                .body(body),
        )
        .await
        .and_then(|reply| transport::business(reply, endpoint));
        if matches!(result, Err(Error::Unauthorized)) {
            self.authorization = None;
        }
        if write {
            result.map_err(|error| transport::mutation(error, endpoint))
        } else {
            result
        }
    }

    async fn data(&mut self, endpoint: &'static str, payload: Value) -> Result<Value> {
        Ok(self.request(endpoint, payload, false).await?["data"].clone())
    }

    /// List vehicles already linked to this official account.
    pub async fn vehicles(&mut self) -> Result<Vec<Vehicle>> {
        let result = self
            .data("vehicleController/getUserIdInfo", json!({}))
            .await?;
        let vehicles: Vec<Vehicle> =
            serde_json::from_value(result).map_err(|_| Error::InvalidResponse)?;
        for vehicle in &vehicles {
            vehicle.validate().map_err(|_| Error::InvalidResponse)?;
        }
        Ok(vehicles)
    }

    /// Read the linked vehicle's official detail.
    pub async fn vehicle(&mut self, id: &Id) -> Result<Value> {
        id.validate()?;
        let detail = self
            .data("vehicleController/getInfo", json!({"vId": id}))
            .await?;
        let returned: Id =
            serde_json::from_value(detail["vId"].clone()).map_err(|_| Error::InvalidResponse)?;
        if !id.same_as(&returned) {
            return Err(Error::InvalidResponse);
        }
        Ok(detail)
    }

    /// Bind a vehicle after the account holder confirms its registration details.
    /// Reconcile against [Self::vehicles] if the outcome is unknown.
    pub async fn add_vehicle(&mut self, fields: &VehicleRegistration<'_>) -> Result<Value> {
        text(fields.plate, "hphm", 32)?;
        text(fields.plate_type, "hpzl", 32)?;
        text(fields.plate_type_name, "hpzlmc", 100)?;
        text(fields.vehicle_type, "cllx", 32)?;
        text(fields.vehicle_type_name, "cllxmc", 100)?;
        text(fields.engine_suffix, "fdjh", 32)?;
        text(fields.model, "ppxh", 32)?;
        text(fields.registration_date, "zcsj", 32)?;
        self.request(
            "/relationController/add",
            json!({"relation": {}, "vehicle": fields}),
            true,
        )
        .await
    }

    /// Read the home screen state and current applications under `bzclxx`.
    pub async fn states(&mut self) -> Result<Value> {
        let data = self
            .data("applyRecordController/stateList", json!({}))
            .await?;
        rows(&data["bzclxx"])?;
        Ok(data)
    }

    pub async fn configuration(&mut self) -> Result<Value> {
        object(
            self.data("configRecordController/getConfigRecordInfo", json!({}))
                .await?,
        )
    }

    pub async fn driver(&mut self) -> Result<Value> {
        let mut driver = object(
            self.data("applyRecordController/getJsrxx", json!({}))
                .await?,
        )?;
        if let Some(kind) = driver.get_mut("rylx") {
            match (kind.as_str(), kind.as_i64()) {
                (Some("0" | "1"), _) => {}
                (_, Some(n @ 0..=1)) => *kind = json!(n.to_string()),
                _ => return Err(Error::InvalidResponse),
            }
        }
        Ok(driver)
    }

    /// Validate official driver and passenger fields. This call can carry personal data.
    pub async fn check_driver(&mut self, fields: &Value) -> Result<Value> {
        let keys = ["jsrxm", "jszh", "dabh", "rylx", "txrxx", "txrkg", "wtxr"];
        let supplied = fields.as_object().ok_or(Error::InvalidInput("driver"))?;
        let mut payload = serde_json::Map::new();
        for key in keys {
            let value = match supplied.get(key) {
                Some(value) => value.clone(),
                None if key == "rylx" => json!("0"),
                None => return Err(Error::InvalidInput(key)),
            };
            payload.insert(key.into(), value);
        }
        let fields = Value::Object(payload);
        if !matches!(fields["rylx"].as_str(), Some("0" | "1")) {
            return Err(Error::InvalidInput("rylx"));
        }
        for key in ["jsrxm", "jszh"] {
            text(
                fields[key].as_str().ok_or(Error::InvalidInput(key))?,
                key,
                128,
            )?;
        }
        validate_passengers(&fields["txrxx"])?;
        self.request("applyRecordController/applyCheckNum", fields, false)
            .await
    }

    pub async fn check_vehicle(&mut self, vehicle: &Vehicle) -> Result<Value> {
        vehicle.validate()?;
        self.request(
            "applyRecordController/applyVehicleCheck",
            json!({"hphm": vehicle.plate, "hpzl": vehicle.plate_type}),
            false,
        )
        .await
    }

    /// Read the official available dates (`jjrqs`) for this vehicle and permit type.
    pub async fn available_dates(&mut self, vehicle: &Vehicle, kind: PermitKind) -> Result<Value> {
        vehicle.validate()?;
        object(
            self.data(
                "applyRecordController/checkHandle",
                json!({"vId": vehicle.id, "jjzzl": kind.code(), "hphm": vehicle.plate}),
            )
            .await?,
        )
    }

    pub async fn route_requirement(&mut self, id: &Id) -> Result<Value> {
        id.validate()?;
        let value = self
            .data(
                "applyRecordController/checkInputRoadInfo",
                json!({"vId": id}),
            )
            .await?;
        if value.is_string() || value.is_i64() {
            Ok(value)
        } else {
            Err(Error::InvalidResponse)
        }
    }

    pub async fn purposes(&mut self) -> Result<Vec<Value>> {
        self.dictionary("jjmd", None).await
    }

    pub async fn districts(&mut self) -> Result<Vec<Value>> {
        self.dictionary("qjxz", None).await
    }

    pub async fn roads(&mut self, id: &Id) -> Result<Vec<Value>> {
        self.dictionary("jjlk", Some(id)).await
    }

    async fn dictionary(&mut self, kind: &str, id: Option<&Id>) -> Result<Vec<Value>> {
        let mut payload = json!({"type": kind});
        if let Some(id) = id {
            id.validate()?;
            payload["vId"] = json!(id);
        }
        rows(&self.data("ucDicController/queryDic", payload).await?)
    }

    /// Submit once after explicit confirmation. Success means the server accepted the request.
    ///
    /// Record a durable intent before invoking this method. On an unknown outcome or cancellation
    /// of the future, use [Self::application_records] to reconcile before taking further action.
    pub async fn submit_application(&mut self, reviewed: Application) -> Result<Value> {
        self.request("/applyRecordController/insertApplyRecord", reviewed.0, true)
            .await
    }

    /// Walk application history for the vehicle in the specified Beijing calendar month.
    pub async fn application_records(
        &mut self,
        vehicle: &Vehicle,
        month: &str,
    ) -> Result<Vec<Value>> {
        vehicle.validate()?;
        let [y0, y1, y2, y3, b'-', m0, m1] = *month.as_bytes() else {
            return Err(Error::InvalidInput("month"));
        };
        if ![y0, y1, y2, y3, m0, m1].iter().all(u8::is_ascii_digit)
            || !matches!((m0, m1), (b'0', b'1'..=b'9') | (b'1', b'0'..=b'2'))
        {
            return Err(Error::InvalidInput("month"));
        }
        let mut records = Vec::new();
        let mut seen = BTreeSet::new();
        for page in 1..=20 {
            let payload = json!({"hpzl": vehicle.plate_type, "hphm": vehicle.plate,
                "bznd": month, "pageNum": page, "pageSize": 10});
            let response = self
                .data("applyRecordController/applyRecordList", payload)
                .await?;
            let table = &response["tableDataInfo"];
            let batch = rows(&table["rows"])?;
            let count = batch.len();
            if count != 0 && !seen.insert(table["rows"].to_string()) {
                return Err(Error::Pagination);
            }
            records.extend(batch);
            let total = table["total"]
                .as_u64()
                .or_else(|| table["total"].as_str()?.parse().ok());
            match total {
                Some(total) if records.len() as u64 >= total => return Ok(records),
                Some(_) if count < 10 => return Err(Error::Pagination),
                None if count < 10 => return Ok(records),
                _ => {}
            }
        }
        Err(Error::Pagination)
    }

    /// Use the application ID and original submission timestamp from the same official record.
    pub async fn application_detail(&mut self, id: &Id, submitted_at: &str) -> Result<Value> {
        let payload = detail_query(id, submitted_at)?;
        let data = self
            .data("applyRecordController/applyDetail", payload)
            .await?;
        rows(&data)?
            .into_iter()
            .next()
            .filter(|detail| detail.get("blzt").is_some())
            .ok_or(Error::InvalidResponse)
    }

    /// Retrieve the electronic permit for the confirmed approved application.
    pub async fn electronic_permit(&mut self, id: &Id, submitted_at: &str) -> Result<Value> {
        let payload = detail_query(id, submitted_at)?;
        object(
            self.data("applyRecordController/cardInfoById", payload)
                .await?,
        )
    }
}

impl Vehicle {
    fn validate(&self) -> Result<()> {
        self.id.validate()?;
        text(&self.plate, "hphm", 32)?;
        text(&self.plate_type, "hpzl", 32)?;
        Ok(())
    }
}

fn detail_query(id: &Id, submitted_at: &str) -> Result<Value> {
    id.validate()?;
    text(submitted_at, "sqsj", 256)?;
    Ok(json!({"applyId": id, "sqsj": submitted_at}))
}

fn rows(data: &Value) -> Result<Vec<Value>> {
    data.as_array()
        .filter(|rows| rows.iter().all(Value::is_object))
        .cloned()
        .ok_or(Error::InvalidResponse)
}

fn object(data: Value) -> Result<Value> {
    if !data.is_object() {
        return Err(Error::InvalidResponse);
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve;
    use axum::{Json, http::StatusCode, response::IntoResponse};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn form() -> Value {
        json!({
            "vId": 42, "hphm": "冀A00000", "hpzl": "02", "cllx": "01",
            "jjzzl": "01", "jsrxm": "合成测试", "jszh": "synthetic-driver",
            "dabh": "", "rylx": "0", "txrxx": [], "jjrq": "2030-01-08",
            "jjmd": "demo-purpose", "jjmdmc": "测试目的", "area": "测试区",
            "jjdq": "demo-district", "xxdz": "测试目的地",
            "sfzj": "0", "jjlk": "demo-road", "jjlkmc": "测试道路", "xslx": "[\"测试路线\"]"
        })
    }

    #[test]
    fn validates_trip_branches_and_rejects_extra_fields() {
        let mut payload = form();
        assert!(Application::from_official_fields(payload.clone()).is_ok());
        let mut trip = form();
        let trip_fields = trip.as_object_mut().unwrap();
        let mut vehicle = json!({});
        let mut driver = json!({});
        for key in ["vId", "hphm", "hpzl", "cllx"] {
            vehicle[key] = trip_fields.remove(key).unwrap();
        }
        for key in ["jsrxm", "jszh", "dabh", "rylx", "txrxx"] {
            driver[key] = trip_fields.remove(key).unwrap();
        }
        trip_fields.remove("jjzzl");
        let assembled =
            Application::from_parts(&vehicle, PermitKind::InsideSixthRing, &driver, trip).unwrap();
        assert_eq!(assembled.0, payload);
        payload["sfzj"] = json!("1");
        payload["zjxxdz"] = json!("在京合成地址");
        payload["zjxxdzgdjd"] = json!("116.123");
        payload["zjxxdzgdwd"] = json!(39.123);
        assert!(Application::from_official_fields(payload.clone()).is_err());
        payload["jjlk"] = json!("");
        payload["jjlkmc"] = json!("");
        assert!(Application::from_official_fields(payload.clone()).is_ok());
        payload["zjxxdzgdwd"] = json!("NaN");
        assert!(Application::from_official_fields(payload).is_err());

        let mut payload = form();
        payload["accessToken"] = json!("synthetic");
        assert!(Application::from_official_fields(payload).is_err());
    }

    #[tokio::test]
    async fn application_workflow_and_uncertain_submission_use_one_official_request_each() {
        let sso_requests = Arc::new(AtomicUsize::new(0));
        let sso = Arc::clone(&sso_requests);
        let submit_requests = Arc::new(AtomicUsize::new(0));
        let submits = Arc::clone(&submit_requests);
        let origin = serve(move |method, uri, headers, bytes| {
            let body = if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap()
            };
            let path = uri.path();
            if path.starts_with("/pro/") {
                assert_eq!(method, "POST");
                assert_eq!(headers["authorization"], "synthetic-business-token");
                assert_eq!(headers["content-type"], "application/json;charset=UTF-8");
            }
            let data = match path {
                "/auth/getSsoUserToken" => {
                    sso.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(method, "GET");
                    assert_eq!(headers["authorization"], "synthetic-app-token");
                    json!("synthetic-sso")
                }
                "/auth/userController/loginUserByUserCenter" => {
                    assert_eq!(method, "POST");
                    assert_eq!(headers["source"], "synthetic-source");
                    assert_eq!(uri.query(), Some("state=synthetic-state"));
                    assert_eq!(
                        body,
                        json!({"token":"synthetic-sso", "state":"synthetic-state"})
                    );
                    json!({"accessToken":"synthetic-business-token"})
                }
                "/pro/vehicleController/getUserIdInfo" => {
                    assert_eq!(body, json!({}));
                    json!([{"vId":42, "hphm":"冀A00000", "hpzl":"02", "cllx":"01"}])
                }
                "/pro//relationController/add" => {
                    assert_eq!(body["relation"], json!({}));
                    assert_eq!(body["vehicle"]["hphm"], "冀A00000");
                    json!({})
                }
                "/pro/applyRecordController/applyVehicleCheck" => {
                    assert_eq!(body, json!({"hphm":"冀A00000", "hpzl":"02"}));
                    json!({})
                }
                "/pro/applyRecordController/getJsrxx" => json!({
                    "jsrxm":"合成测试","jszh":"synthetic-driver","dabh":"", "rylx":"0"
                }),
                "/pro/applyRecordController/applyCheckNum" => {
                    assert_eq!(body["rylx"], "0");
                    assert_eq!(body["txrxx"], json!([]));
                    json!({})
                }
                "/pro/applyRecordController/checkHandle" => {
                    assert_eq!(body, json!({"vId":42, "jjzzl":"01", "hphm":"冀A00000"}));
                    json!({"jjrqs":["2030-01-08"]})
                }
                "/pro/ucDicController/queryDic" => {
                    assert_eq!(body, json!({"type":"jjlk","vId":42}));
                    json!([{"zdbm":"demo-road", "zdz":"测试道路"}])
                }
                "/pro//applyRecordController/insertApplyRecord" => {
                    assert_eq!(body, form());
                    if submits.fetch_add(1, Ordering::SeqCst) != 0 {
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    json!({"cgts":["已受理"]})
                }
                "/pro/applyRecordController/applyRecordList" => {
                    assert_eq!(body["bznd"], "2030-01");
                    assert_eq!(body["pageSize"], 10);
                    let page = body["pageNum"].as_u64().unwrap();
                    assert!((1..=2).contains(&page));
                    let rows: Vec<_> = if page == 1 {
                        (1..=10)
                            .map(|i| json!({"apply_id":i,"sqsj":"first-page"}))
                            .collect()
                    } else {
                        vec![json!({"apply_id":11,"sqsj":"second-page"})]
                    };
                    json!({"tableDataInfo":{"rows":rows,"total":11}})
                }
                "/pro/applyRecordController/applyDetail" => {
                    assert_eq!(body, json!({"applyId":11,"sqsj":"second-page"}));
                    json!([{"blzt":"5"}])
                }
                _ => panic!("unexpected path {path}"),
            };
            Json(json!({"code":200,"data":data})).into_response()
        })
        .await;
        let mut client = PermitClient::new(
            AppSession::from_token("synthetic-app-token").unwrap(),
            "synthetic-state",
            "synthetic-source",
        )
        .unwrap();
        client.app_origin = origin.clone();
        client.permit_origin = origin;
        client.http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap();

        client
            .add_vehicle(&VehicleRegistration {
                plate: "冀A00000",
                plate_type: "02",
                plate_type_name: "小型汽车",
                vehicle_type: "01",
                vehicle_type_name: "客车",
                engine_suffix: "123456",
                model: "TEST",
                registration_date: "2030-01-01",
                truck_subtype: "",
            })
            .await
            .unwrap();
        let vehicles = client.vehicles().await.unwrap();
        let vehicle = &vehicles[0];
        client.check_vehicle(vehicle).await.unwrap();
        let mut driver = client.driver().await.unwrap();
        driver["txrxx"] = json!([]);
        driver["txrkg"] = json!("0");
        driver["wtxr"] = json!("");
        client.check_driver(&driver).await.unwrap();
        assert_eq!(
            client
                .available_dates(vehicle, PermitKind::InsideSixthRing)
                .await
                .unwrap()["jjrqs"][0],
            "2030-01-08"
        );
        assert_eq!(
            client.roads(&vehicle.id).await.unwrap()[0]["zdbm"],
            "demo-road"
        );
        let reply = client
            .submit_application(Application::from_official_fields(form()).unwrap())
            .await
            .unwrap();
        assert_eq!(reply["data"]["cgts"][0], "已受理");
        assert_eq!(submit_requests.load(Ordering::SeqCst), 1);
        assert!(matches!(
            client
                .submit_application(Application::from_official_fields(form()).unwrap())
                .await,
            Err(Error::OutcomeUnknown(
                "/applyRecordController/insertApplyRecord"
            ))
        ));
        assert_eq!(submit_requests.load(Ordering::SeqCst), 2);
        let records = client
            .application_records(vehicle, "2030-01")
            .await
            .unwrap();
        assert_eq!(records.len(), 11);
        let id: Id = serde_json::from_value(records[10]["apply_id"].clone()).unwrap();
        let detail = client
            .application_detail(&id, records[10]["sqsj"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(detail["blzt"], "5");
        assert_eq!(sso_requests.load(Ordering::SeqCst), 1);
    }
}
