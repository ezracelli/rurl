use anyhow::Result;
use hyper::{
    body::HttpBody as _,
    header::{HeaderName, HeaderValue},
    Body, Client, Method, Request, Uri,
};
use json::JsonValue;
use std::str::FromStr as _;
use structopt::StructOpt;
use thiserror::Error;

#[derive(Debug, Error)]
enum ModeError {
    #[error("Missing mode options")]
    MissingMode,
}

#[derive(Debug)]
enum Mode {
    Form,
    Json,
}

impl Default for Mode {
    fn default() -> Self {
        Self::Json
    }
}

impl std::str::FromStr for Mode {
    type Err = ModeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "form" => Ok(Self::Form),
            "json" => Ok(Self::Json),
            _ => Err(Self::Err::MissingMode),
        }
    }
}

#[derive(Debug)]
enum RequestItem {
    Data {
        key: String,
        value: String,
    },
    FormFile {
        key: String,
        value: std::path::PathBuf,
    },
    Header {
        key: HeaderName,
        value: HeaderValue,
    },
    JsonData {
        key: String,
        value: JsonValue,
    },
    SearchParam {
        key: String,
        value: String,
    },
}

#[derive(Debug, Error)]
enum RequestItemError {
    #[error("could not parse request item {0}")]
    ParseError(String),
    #[error("unknown request item variant {0}")]
    VariantParseError(String),
    #[error("missing file input {0}")]
    MissingFileInputError(String),
    #[error("could not read file {0}")]
    IOError(String),
}

impl std::str::FromStr for RequestItem {
    type Err = RequestItemError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use onig::*;

        lazy_static::lazy_static! {
            static ref RE: Regex = Regex::new(r"(?x)
                (?<name>.+?)
                (?<!\\)
                (?<sep>==|:=@?|=@?|:|@)
                (?<value>.*)
            ").unwrap();
        }

        match RE.captures(s) {
            Some(captures) => {
                let key: String = captures.at(1).unwrap().into();
                let mut value: String = captures.at(3).unwrap().into();

                let request_item = {
                    let mut variant: String = captures.at(2).unwrap().into();

                    if variant.len() > 1 && variant.ends_with("@") {
                        use std::io::Read;

                        if value.len() == 0 {
                            return Err(Self::Err::MissingFileInputError(value));
                        }

                        let mut file = std::fs::File::open(value.clone())
                            .or(Err(Self::Err::IOError(value.clone())))?;

                        let mut buf = String::new();
                        file.read_to_string(&mut buf)
                            .or(Err(Self::Err::IOError(value.clone())))?;

                        value = buf;
                        variant = variant.replace("@", "");
                    }

                    match variant.as_str() {
                        "=" => Self::Data { key, value },
                        "@" => Self::FormFile {
                            key,
                            value: value.parse().or(Err(Self::Err::ParseError(s.into())))?,
                        },
                        ":" => Self::Header {
                            key: key.parse().or(Err(Self::Err::ParseError(s.into())))?,
                            value: value.parse().or(Err(Self::Err::ParseError(s.into())))?,
                        },
                        ":=" => Self::JsonData {
                            key,
                            value: json::parse(&value).or(Err(Self::Err::ParseError(s.into())))?,
                        },
                        "==" => Self::SearchParam { key, value },
                        _ => return Err(Self::Err::VariantParseError(variant.into())),
                    }
                };

                Ok(request_item)
            }
            None => Err(Self::Err::ParseError(s.into())),
        }
    }
}

#[derive(Debug, structopt::StructOpt)]
struct Opt {
    #[structopt(short, long, conflicts_with = "json")]
    form: bool,

    #[structopt(short, long, conflicts_with = "form")]
    json: bool,

    #[structopt(
        short,
        long,
        hidden(true),
        default_value_if("form", None, "form"),
        default_value_if("json", None, "json")
    )]
    mode: Option<Mode>,

    #[structopt(name = "METHOD")]
    method: Method,

    #[structopt(name = "URI")]
    uri: Uri,

    #[structopt(name = "REQUEST_ITEM")]
    request_items: Vec<RequestItem>,
}

fn highlight(input: &str, language: &str) -> String {
    use syntect::{
        easy::HighlightLines,
        highlighting::{Style, ThemeSet},
        parsing::{syntax_definition::SyntaxDefinition, SyntaxSet},
        util::LinesWithEndings,
    };

    let mut ps = SyntaxSet::load_defaults_newlines().into_builder();
    ps.add(
        SyntaxDefinition::load_from_str(
            include_str!("../syntaxes/http-response.sublime-syntax",),
            true,
            None,
        )
        .unwrap(),
    );

    let ps = ps.build();
    let ts = ThemeSet::load_defaults();

    let syntax = ps.find_syntax_by_extension(language).unwrap();
    let mut higlighter = HighlightLines::new(syntax, &ts.themes["base16-ocean.dark"]);

    let lines = LinesWithEndings::from(input);

    lines
        .map(|line| {
            let ranges: Vec<(Style, &str)> = higlighter.highlight(line, &ps);
            syntect::util::as_24_bit_terminal_escaped(&ranges[..], false)
        })
        .collect::<String>()
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::from_args();

    println!("{:#?}\n", opt);

    // build request

    let client = Client::new();

    let mut req = Request::builder()
        .method(opt.method)
        .uri({
            use hyper::http::uri::*;

            let mut parts = opt.uri.into_parts();

            match parts.scheme {
                Some(_) => {}
                None => parts.scheme = Some(Scheme::HTTP),
            }

            match parts.path_and_query {
                Some(_) => {}
                None => parts.path_and_query = Some(PathAndQuery::from_str("/")?),
            }

            Uri::from_parts(parts)?
        })
        .header("accept", mime::STAR_STAR.to_string())
        .header(
            "user-agent",
            format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        );

    // build request headers

    for request_item in opt.request_items.iter() {
        if let RequestItem::Header { key, value } = request_item {
            req = req.header(key, value);
        }
    }

    // build request body

    let body = {
        match opt.mode {
            Some(Mode::Json) | None => {
                let body_items_len =
                    opt.request_items
                        .iter()
                        .fold(0, |prev, request_item| match request_item {
                            RequestItem::Data { key: _, value: _ } => prev + 1,
                            RequestItem::JsonData { key: _, value: _ } => prev + 1,
                            _ => prev,
                        });

                if body_items_len > 0 {
                    let mut body = JsonValue::new_object();

                    for request_item in opt.request_items {
                        if let RequestItem::Data { key, value } = request_item {
                            body[key] = json::parse(&json::stringify(value))?;
                        } else if let RequestItem::JsonData { key, value } = request_item {
                            body[key] = value;
                        }
                    }

                    let body = body.dump();

                    req = req
                        .header("content-type", mime::APPLICATION_JSON.to_string())
                        .header("content-length", body.len());

                    body
                } else {
                    "".into()
                }
            }
            Some(Mode::Form) => {
                let mut body = String::new();

                for request_item in opt.request_items {
                    if let RequestItem::Data { key, value } = request_item {
                        if !body.is_empty() {
                            body.push('&');
                        }

                        body.push_str(&urlencoding::encode(&key));
                        body.push('=');
                        body.push_str(&urlencoding::encode(&value));
                    } else if let RequestItem::JsonData { key, value } = request_item {
                        if !body.is_empty() {
                            body.push('&');
                        }

                        body.push_str(&urlencoding::encode(&key));
                        body.push('=');
                        body.push_str(&urlencoding::encode(&value.dump()));
                    }
                }

                if body.len() > 0 {
                    req = req
                        .header(
                            "content-type",
                            mime::APPLICATION_WWW_FORM_URLENCODED_UTF_8.to_string(),
                        )
                        .header("content-length", body.len());
                }

                body
            }
        }
    };

    let req = req.body(Body::from(body.clone()))?;

    // print request

    let mut request = format!(
        "{} {} {:?}\n",
        req.method(),
        req.uri().path(),
        req.version()
    );

    // print request headers

    let mut headers: Vec<(&HeaderName, &HeaderValue)> = req.headers().iter().collect();
    headers.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));

    for (name, value) in headers {
        request += &format!("{}: {}\n", name, std::str::from_utf8(value.as_ref())?);
    }

    eprintln!("{}", highlight(&request, "http"));

    // print request body

    if body.len() > 0 {
        eprintln!(
            "{}\x1b[0m",
            highlight(
                &body,
                match opt.mode {
                    Some(Mode::Json) | None => "json",
                    Some(Mode::Form) => "json",
                }
            )
        );

        if !body.ends_with("\n") {
            eprintln!("");
        }
    }

    // make request

    let mut res = client.request(req).await?;

    // print response

    let mut response = format!(
        "{:?} {} {}\n",
        res.version(),
        res.status().as_u16(),
        res.status().canonical_reason().unwrap()
    );

    // print response headers

    let mut headers: Vec<(&HeaderName, &HeaderValue)> = res.headers().iter().collect();
    headers.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));

    for (name, value) in headers {
        response += &format!("{}: {}\n", name, std::str::from_utf8(value.as_ref())?);
    }

    eprintln!("{}", highlight(&response, "http"));

    // get response body

    let mut buf = Vec::new();
    while let Some(chunk) = res.body_mut().data().await {
        buf.append(&mut Vec::from(chunk?.as_ref()));
    }

    let body = std::str::from_utf8(&buf)?;

    // print response body

    let body = match res.headers().get("content-type") {
        Some(header) => {
            let mime: mime::MediaType = std::str::from_utf8(header.as_ref())?.parse()?;

            match (mime.type_(), mime.subtype()) {
                (mime::TEXT, mime::HTML) => highlight(body, "html"),
                (mime::APPLICATION, mime::JSON) => highlight(body, "json"),
                _ => body.into(),
            }
        }
        None => body.into(),
    };

    if body.len() > 0 {
        println!("{}\x1b[0m", body);

        if !body.ends_with("\n") {
            eprintln!("");
        }
    }

    Ok(())
}
