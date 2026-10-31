//! Autodiscover through the real router.

mod support;

use support::{Harness, fake_jmap};

fn request(address: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/mobilesync/requestschema/2006">
<Request><EMailAddress>{address}</EMailAddress>
<AcceptableResponseSchema>http://schemas.microsoft.com/exchange/autodiscover/mobilesync/responseschema/2006</AcceptableResponseSchema>
</Request></Autodiscover>"#
    )
}

#[tokio::test]
async fn hands_out_the_eas_url_after_checking_the_password() {
    let h = Harness::start().await;
    let http = tolk::http_client().build().unwrap();
    let url = format!("{}/Autodiscover/Autodiscover.xml", h.url);

    let anonymous = http
        .post(&url)
        .body(request(fake_jmap::USER))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);
    assert!(anonymous.headers().contains_key("www-authenticate"));

    let wrong = http
        .post(&url)
        .basic_auth(fake_jmap::USER, Some("wrong"))
        .body(request(fake_jmap::USER))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    let reply = http
        .post(&url)
        .basic_auth(fake_jmap::USER, Some(fake_jmap::PASSWORD))
        .body(request(fake_jmap::USER))
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    let xml = reply.text().await.unwrap();
    let host = h.url.trim_start_matches("http://");
    assert!(
        xml.contains(&format!(
            "<Url>https://{host}/Microsoft-Server-ActiveSync</Url>"
        )),
        "{xml}"
    );
    assert!(xml.contains(&format!("<EMailAddress>{}</EMailAddress>", fake_jmap::USER)));
}

#[tokio::test]
async fn refuses_outlook_desktop_schemas() {
    let h = Harness::start().await;
    let body = request(fake_jmap::USER).replace(
        "mobilesync/responseschema/2006",
        "outlook/responseschema/2006a",
    );
    let xml = tolk::http_client()
        .build()
        .unwrap()
        .post(format!("{}/autodiscover/autodiscover.xml", h.url))
        .basic_auth(fake_jmap::USER, Some(fake_jmap::PASSWORD))
        .body(body)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(xml.contains("<Status>601</Status>"), "{xml}");
}

#[tokio::test]
async fn json_endpoint_names_the_url() {
    let h = Harness::start().await;
    let reply: serde_json::Value = reqwest::get(format!(
        "{}/autodiscover/autodiscover.json/v1.0/{}?Protocol=ActiveSync",
        h.url,
        fake_jmap::USER
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(reply["Protocol"], "ActiveSync");
    assert!(
        reply["Url"]
            .as_str()
            .unwrap()
            .ends_with("/Microsoft-Server-ActiveSync")
    );

    let other = reqwest::get(format!(
        "{}/autodiscover/autodiscover.json/v1.0/{}?Protocol=Ews",
        h.url,
        fake_jmap::USER
    ))
    .await
    .unwrap();
    assert_eq!(other.status(), 400);
}
