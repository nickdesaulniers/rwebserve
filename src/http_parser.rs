import io;
import io::writer_util;
import rparse::*;
import rparse::misc::*;
import rparse::types::*;
import std::map;
import std::map::hashmap;

export header_map, http_request, make_parser;

type header_map = map::hashmap<str, str>;

type http_request = {
	method: str,
	major_version: int,
	minor_version: int,
	url: str,
	headers: header_map,
	body: str};

// TODO: 
// currently we only support GET methods
// Server, User-Agent, and Via values can have comments
// double quotes can be used with header values that use separators
fn request_parser() -> parser<http_request>
{
	let space1 = literal(" ");
	let tab1 = literal("\t");
	let space = literal(" ").repeat0();
	let dot = literal(".");
	let crnl = literal("\r\n").tag("Expected CRNL");
	
	// url := [^ ]+
	let url = match1({|c| c != ' '}, "Expected an URL");
	
	// version := integer '.' integer
	let version = sequence3(integer(), dot, integer())
		{|major, _a2, minor| result::ok((major, minor))};
		
	// get_method := 'GET ' url space 'HTTP/' version crnl
	let get_method = sequence6(literal("GET "), url, space, literal("HTTP/"), version, crnl)
		{|_a1, url, _a3, _a4, version, _a6| result::ok((url, version))};
		
	// value := [^\r\n]+
	// continuation := crnl [ \t] value
	let value = match1({|c| c != '\r' && c != '\n'}, "Expected a header value");
	let continuation = sequence3(crnl, space1.or(tab1), value)
		{|_a1, _a2, v| result::ok(" " + str::trim(v))};
	
	// name := [^:]+
	// header := name ': ' value continuation* crnl
	// headers := header*
	let name = match1({|c| c != ':'}, "Expected a header name");
	let header = sequence5(name, literal(":"), value, continuation.repeat0(), crnl)
		{|n, _a2, v, cnt, _a5| result::ok((str::to_lower(n), str::trim(v) + str::connect(cnt, "")))};	// 4.2 says that header names are case-insensitive so we lower case them
	let headers = header.repeat0();
	
	// request := get_method headers crnl
	let request = sequence3(get_method, headers, crnl)
		{|a1, h, _a2|
			let (u, (v1, v2)) = a1;
			let entries = std::map::str_hash::<str>();
			vec::iter(h)
			{|entry|
				let (n, v) = entry;
				entries.insert(n, v);
			};
			result::ok({method: "GET", major_version: v1, minor_version: v2, url: u, headers: entries, body: ""})};
	
	ret request;
}

// We return a closure so that we can build the parser just once.
fn make_parser() -> fn@ (str) -> result::result<http_request, str>
{
	{|request: str|
		let parser = request_parser();
		result::chain_err(parse(parser, "http request", request))
		{|err|
			result::err(#fmt["%s on line %? col %?", err.mesg, err.line, err.col])
		}
	}
}

#[cfg(test)]
fn equal<T: copy>(result: T, expected: T) -> bool
{
	if result != expected
	{
		io::stderr().write_line(#fmt["Expected %? but found %?", expected, result]);
		ret false;
	}
	ret true;
}

#[test]
fn test_get_method1()
{
	let p = make_parser();
	
	alt p("GET / HTTP/1.1\r\n\r\n")
	{
		result::ok(value)
		{
			assert equal(value.method, "GET");
			assert equal(value.major_version, 1);
			assert equal(value.minor_version, 1);
			assert equal(value.url, "/");
			assert equal(value.headers.size(), 0u);
			assert equal(str::len(value.body), 0u);
		}
		result::err(mesg)
		{
			io::stderr().write_line(mesg);
			assert false;
		}
	}
}

#[test]
fn test_get_method2()
{
	let p = make_parser();
	
	alt p("GET / HTTP/1.1\r\nHost: localhost:8080\r\nUser-Agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10.7; rv:11.0) Gecko/20100101 Firefox/11.0\r\nAccept: text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8\r\nAccept-Language: en-us,en;q=0.5\r\nAccept-Encoding: gzip, deflate\r\nConnection: keep-alive\r\n\r\n")
	{
		result::ok(value)
		{
			assert equal(value.method, "GET");
			assert equal(value.major_version, 1);
			assert equal(value.minor_version, 1);
			assert equal(value.url, "/");
			assert equal(value.headers.size(), 6u);
			assert equal(str::len(value.body), 0u);
			
			assert equal(value.headers.get("host"), "localhost:8080");
			assert equal(value.headers.get("user-agent"), "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.7; rv:11.0) Gecko/20100101 Firefox/11.0");
			assert equal(value.headers.get("accept"), "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8");
			assert equal(value.headers.get("accept-language"), "en-us,en;q=0.5");
			assert equal(value.headers.get("accept-encoding"), "gzip, deflate");
			assert equal(value.headers.get("connection"), "keep-alive");
		}
		result::err(mesg)
		{
			io::stderr().write_line(mesg);
			assert false;
		}
	}
}

#[test]
fn test_unknown_method()
{
	let p = make_parser();
	
	alt p("GET / HXTP/1.1\r\n\r\n")
	{
		result::ok(value)
		{
			io::stderr().write_line(#fmt["Somehow parsed %?", value]);
			assert false;
		}
		result::err(mesg)
		{
			assert equal(mesg, "Expected 'HTTP/' on line 1 col 8");
		}
	}
}

#[test]
fn test_header_values()
{
	let p = make_parser();
	
	alt p("GET / HTTP/1.1\r\nHost:   \t xxx\r\nBlah:   \t bbb \t\r\nMulti: line1\r\n  \tline2\r\n  line3\r\n\r\n")
	{
		result::ok(value)
		{
			assert equal(value.headers.get("host"), "xxx");
			assert equal(value.headers.get("blah"), "bbb");
			assert equal(value.headers.get("multi"), "line1 line2 line3");
		}
		result::err(mesg)
		{
			io::stderr().write_line(mesg);
			assert false;
		}
	}
}
