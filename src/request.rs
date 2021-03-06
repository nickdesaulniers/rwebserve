//! Handles an incoming request from a client connection and sends a response.
use io::WriterUtil;
use http_parser::{HttpRequest};

// TODO:
// include last-modified and maybe etag
pub fn process_request(config: &connection::ConnConfig, request: &HttpRequest, local_addr: &str, remote_addr: &str) -> (~str, Body)
{
	info!("Servicing %s for %s", request.method, utils::truncate_str(request.url, 80));
	
	let version = fmt!("%d.%d", request.major_version, request.minor_version);
	let (path, params) = parse_url(request.url);
	let request = Request {version: version, method: request.method, local_addr: local_addr.to_unique(), remote_addr: remote_addr.to_unique(), path: path, matches: std::map::HashMap(), params: params, headers: utils::to_boxed_str_hash(request.headers), body: request.body};
	let types = if request.headers.contains_key(@~"accept") {str::split_char(*request.headers.get(@~"accept"), ',')} else {~[~"text/html"]};
	let (response, body) = get_body(config, &request, types);
	
	let (header, body) = make_header_and_body(&response, body);
	debug!("response header: %s", header);
	debug!("response body: %?", body);		// TODO: only print this if its a text mime type (and convert it to a str if so)
	
	(header, body)
}

priv fn parse_url(url: &str) -> (~str, IMap<@~str, @~str>)
{
	match str::find_char(url, '?')
	{
		option::Some(i) =>
		{
			let query = str::slice(url, i+1, str::len(url));
			let parts = str::split_char(query, '&');
			
			let params = do vec::map(parts)
			|p|
			{
				match str::find_char(*p, '=')
				{
					option::Some(i) =>
					{
						~[@p.slice(0, i), @p.slice(i+1, p.len())]
					}
					option::None =>
					{
						~[@p.to_unique()]		// bad field
					}
				}
			};
			
			if do vec::all(params) |p| {vec::len(*p) == 2}
			{
				(str::slice(url, 0, i), do vec::map(params) |p| {(p[0], p[1])})
			}
			else
			{
				// It's not a valid query string so we'll just let the server handle it.
				// Presumbably it won't match any routes so we'll get an error then.
				error!("invalid query string");
				(url.to_unique(), ~[])
			}
		}
		option::None =>
		{
			(url.to_unique(), ~[])
		}
	}
}

pub fn make_initial_response(config: &connection::ConnConfig, status_code: ~str, status_mesg: ~str, mime_type: ~str, request: &Request) -> Response
{
	let headers = utils::to_boxed_str_hash(~[
		(~"Content-Type", copy mime_type),
		(~"Date", std::time::now_utc().rfc822()),
		(~"Server", copy config.server_info),
	]);
	
	if config.settings.contains_key(@~"debug") && config.settings.get(@~"debug") == @~"true"
	{
		headers.insert(@~"Cache-Control", @~"no-cache");
	}
	
	let context = std::map::HashMap();
	context.insert(@~"request-path", mustache::Str(@copy request.path));
	context.insert(@~"status-code", mustache::Str(@copy status_code));
	context.insert(@~"status-mesg", mustache::Str(@copy status_mesg));
	context.insert(@~"request-version", mustache::Str(@copy request.version));
	
	Response {status: status_code + ~" " + status_mesg, headers: headers, body: StringBody(@~""), template: ~"", context: context}
}

priv fn body_len(body: &Body) -> uint
{
	match *body
	{
		StringBody(text) =>
		{
			text.len()
		}
		BinaryBody(binary) =>
		{
			binary.len()
		}
		CompoundBody(parts) =>
		{
			do parts.foldl(0) |result, part| {*result + body_len(*part)}
		}
	}
}

pub fn make_header_and_body(response: &Response, body: Body) -> (~str, Body)
{
	let mut headers = ~"";
	let mut has_content_len = false;
	let mut is_chunked = false;
	
	for response.headers.each()
	|name, value|
	{
		if *name == ~"Content-Length"
		{
			has_content_len = true;
		}
		else if *name == ~"Transfer-Encoding" && *value == ~"chunked"
		{
			is_chunked = true;
		}
		
		if *name == ~"Content-Length" && *value == ~"0"
		{
			headers += fmt!("Content-Length: %?\r\n", body_len(&body));
		}
		else
		{
			headers += fmt!("%s: %s\r\n", *name, *value);
		}
	};
	
	if is_chunked
	{
		assert !has_content_len;
	}
	else if !has_content_len
	{
		headers += fmt!("Content-Length: %?\r\n", body_len(&body));
	}
	
	(
		fmt!("HTTP/1.1 %s\r\n%s\r\n", response.status, headers),
		if is_chunked
		{
			CompoundBody(@[@StringBody(@fmt!("%X\r\n", body_len(&body))), @body, @StringBody(@~"\r\n")])
		}
		else
		{
			body
		}
	)
}

priv fn get_body(config: &connection::ConnConfig, request: &Request, types: ~[~str]) -> (Response, Body)
{
	if vec::contains(types, &~"text/event-stream") 
	{
		process_sse(config, request)
	}
	else
	{
		let (status_code, status_mesg, mime_type, handler, matches) = find_handler(config, request.method, request.path, types, request.version);
		
		let response = make_initial_response(config, status_code, status_mesg, mime_type, request);
		let response = handler(config, &Request {matches: matches, ..*request}, &response);
		
		if str::is_not_empty(response.template.to_str())
		{
			process_template(config, &response, request)
		}
		else
		{
			let body = response.body;
			(response, body)
		}
	}
}

priv fn find_handler(config: &connection::ConnConfig, method: &str, request_path: &str, types: &[~str], version: &str) -> (~str, ~str, ~str, ResponseHandler, HashMap<@~str, @~str>)
{
	let mut handler = option::None;
	let mut status_code = ~"200";
	let mut status_mesg = ~"OK";
	let mut result_type = ~"text/html; charset=UTF-8";
	let mut matches = std::map::HashMap();
	
	// According to section 3.1 servers are supposed to accept new minor version editions.
	if !str::starts_with(version, "1.")
	{
		status_code = ~"505";
		status_mesg = ~"HTTP Version Not Supported";
		let (_, _, _, h, _) = find_handler(config, method, ~"not-supported.html", ~[~"types/html"], ~"1.1");
		handler = option::Some(h);
		info!("responding with %s %s", status_code, status_mesg);
	}
	
	// See if the url matches a file under the resource root (i.e. the url can't have too many .. components).
	if option::is_none(&handler)
	{
		let path = utils::url_to_path(&config.resources_root, request_path);
		let path = path.normalize();
		if str::starts_with(path.to_str(), config.resources_root.to_str())
		{
			if config.valid_rsrc(&path)
			{
				let mime_type = path_to_type(config, request_path);
				if vec::contains(types, &~"*/*") || vec::contains(types, &mime_type)
				{
					result_type = mime_type + ~"; charset=UTF-8";
					handler = option::Some(copy config.static_handler);
				}
			}
		}
		else
		{
			status_code = ~"403";			// don't allow access to files not under resources_root
			status_mesg = ~"Forbidden";
			let (_, _, _, h, _) = find_handler(config, method, ~"forbidden.html", ~[~"types/html"], version);
			handler = option::Some(h);
			info!("responding with %s %s (path wasn't under resources_root)", status_code, status_mesg);
		}
	}
	
	// Then look for the first matching route.
	if option::is_none(&handler)
	{
		for vec::each(config.route_list)
		|entry|
		{
			if str::eq_slice(entry.method, method)
			{
				let m = uri_template::match_template(request_path, entry.template);
				if m.size() > 0u
				{
					if vec::contains(types, &entry.mime_type)
					{
						handler = option::Some(config.views_table.get(@copy entry.route));
						result_type = entry.mime_type + ~"; charset=UTF-8";
						matches = m;
						break;
					}
					else
					{
						info!("request matches route but route type is %s not one of: %s", entry.mime_type, str::connect(types, ~", "));
					}
				}
			}
		}
	}
	
	// Otherwise use the missing handler.
	if option::is_none(&handler)
	{
		status_code = ~"404";
		status_mesg = ~"Not Found";
		handler = option::Some(copy(config.missing));
		info!("responding with %s %s", status_code, status_mesg);
	}
	
	return (status_code, status_mesg, result_type, option::get(&handler), matches);
}

priv fn load_template(config: &connection::ConnConfig, path: &Path) -> result::Result<@~str, ~str>
{
	// {{ should be followed by }} (rust-mustache hangs if this is not the case).
	fn match_curly_braces(text: &str) -> bool
	{
		let mut index = 0u;
		
		while index < str::len(text)
		{
			match str::find_str_from(text, "{{", index)
			{
				option::Some(i) =>
				{
					match str::find_str_from(text, "}}", i + 2u)
					{
						option::Some(j) =>
						{
							index = j + 2u;
						}
						option::None() =>
						{
							return false;
						}
					}
				}
				option::None =>
				{
					break;
				}
			}
		}
		return true;
	}
	
	do result::chain(config.load_rsrc(path))
	|template|
	{
		let template = str::from_bytes(template);
		if !config.settings.contains_key(@~"debug") || config.settings.get(@~"debug") == @~"false" || match_curly_braces(template)
		{
			result::Ok(@template)
		}
		else
		{
			result::Err(~"mismatched curly braces")
		}
	}
}

priv fn process_template(config: &connection::ConnConfig, response: &Response, request: &Request) -> (Response, Body)
{
	let path = utils::url_to_path(&config.resources_root, response.template);
	let (response, body) =
		match load_template(config, &path)
		{
			result::Ok(v) =>
			{
				// We found a legit template file.
				(Response {status: response.status, ..*response}, v)		// hacky way to return a new Response without a copy
			}
			result::Err(ref mesg) =>
			{
				// We failed to load the template so use the hard-coded config.read_error body.
				let context = std::map::HashMap();
				context.insert(@~"request-path", mustache::Str(@copy request.path));
				let body = mustache::compile_str(config.read_error).render_data(mustache::Map(context));
				
				if config.server_info != ~"unit test"
				{
					error!("Error '%s' tying to read '%s'", *mesg, path.to_str());
				}
				(make_initial_response(config, ~"403", ~"Forbidden", ~"text/html; charset=UTF-8", request), @body)
			}
		};
	
	if !str::starts_with(response.status, "403") && response.context.size() > 0u
	{
		// If we were able to load a template, and we have context, then use the
		// context to expand the template.
		let base_dir = url_dirname(response.template);
		let base_url = fmt!("http://%s:%?/%s/", request.local_addr, config.port, base_dir);
		response.context.insert(@~"base-path", mustache::Str(@base_url));
		
		let body = mustache::compile_str(*body).render_data(mustache::Map(response.context));
		(response, StringBody(@body))
	}
	else
	{
		(response, StringBody(body))
	}
}

priv fn url_dirname(path: &str) -> ~str
{
	match str::find_char(path, '/')
	{
		option::Some(index) 	=> path.slice(0, index+1),
		option::None			=> path.to_unique(),
	}
}

priv fn path_to_type(config: &connection::ConnConfig, path: &str) -> ~str
{
	let p: path::Path = path::from_str(path);
	let extension: Option<~str> = p.filetype();
	if extension.is_some()
	{
		assert extension.get().char_at(0) == '.';
		
		match config.static_type_table.find(@extension.get())
		{
			option::Some(v) =>
			{
				copy *v
			}
			option::None =>
			{
				warn!("Couldn't find a static_types entry for %s", path);
				~"text/html"
			}
		}
	}
	else
	{
		warn!("Can't determine mime type for %s", path);
		~"text/html"
	}
}

#[cfg(test)]
fn test_view(_config: &connection::ConnConfig, _request: &Request, response: &Response) -> Response
{
	Response {template: ~"test.html", ..*response}
}

#[cfg(test)]
fn null_loader(path: &Path) -> result::Result<~[u8], ~str>
{
	result::Ok(str::to_bytes(path.to_str() + ~" contents"))
}

#[cfg(test)]
fn err_loader(path: &Path) -> result::Result<~[u8], ~str>
{
	result::Err(path.to_str() + ~" failed to load")
}

#[cfg(test)]
fn make_request(url: ~str, mime_type: ~str) -> HttpRequest
{
	let headers = ~[		// http_parser lower cases header names so we do too
		(~"host", ~"localhost:8080"),
		(~"user-agent", ~"Mozilla/5.0"),
		(~"accept", copy mime_type),
		(~"accept-Language", ~"en-us,en"),
		(~"accept-encoding", ~"gzip, deflate"),
		(~"connection", ~"keep-alive")];
	HttpRequest {method: ~"GET", major_version: 1, minor_version: 1, url: url, headers: headers, body: ~""}
}

#[test]
fn html_route()
{
	let config = Config {
		hosts: ~[~"localhost"],
		server_info: ~"unit test",
		resources_root: path::from_str(~"server/html"),
		routes: ~[(~"GET", ~"/foo/bar", ~"foo")],
		views: ~[(~"foo",  test_view)],
		load_rsrc: null_loader
		, .. initialize_config()};
		
	let eport = comm::Port();
	let ech = comm::Chan(&eport);
	let iconfig = connection::config_to_conn(&config, ech);
	
	let request = make_request(~"/foo/bar", ~"text/html");
	let (_header, body) = process_request(&iconfig, &request, ~"10.11.12.13", ~"1.2.3.4");
	
	assert body.to_str() == ~"server/html/test.html contents";
}

#[test]
fn route_with_bad_type()
{
	let config = Config {
		hosts: ~[~"localhost"],
		server_info: ~"unit test",
		resources_root: path::from_str(~"server/html"),
		routes: ~[(~"GET", ~"/foo/bar", ~"foo")],
		views: ~[(~"foo",  test_view)],
		load_rsrc: null_loader
		, .. initialize_config()};
		
	let eport = comm::Port();
	let ech = comm::Chan(&eport);
	let iconfig = connection::config_to_conn(&config, ech);
	
	let request = make_request(~"/foo/bar", ~"text/zzz");
	let (header, body) = process_request(&iconfig, &request, ~"10.11.12.13", ~"1.2.3.4");
	
	assert header.contains("404 Not Found");
	assert header.contains("Content-Type: text/html");
	assert body.to_str() == ~"server/html/not-found.html contents";
}

#[test]
fn non_html_route()
{
	let config = Config {
		hosts: ~[~"localhost"],
		server_info: ~"unit test",
		resources_root: path::from_str(~"server/html"),
		routes: ~[(~"GET", ~"/foo/bar<text/csv>", ~"foo")],
		views: ~[(~"foo",  test_view)],
		load_rsrc: null_loader
		, ..initialize_config()};
		
	let eport = comm::Port();
	let ech = comm::Chan(&eport);
	let iconfig = connection::config_to_conn(&config, ech);
	
	let request = make_request(~"/foo/bar", ~"text/csv");
	let (_header, body) = process_request(&iconfig, &request, ~"10.11.12.13", ~"1.2.3.4");
	
	assert body.to_str() == ~"server/html/test.html contents";
}

#[test]
fn static_route()
{
	let config = Config {
		hosts: ~[~"localhost"],
		server_info: ~"unit test",
		resources_root: path::from_str(~"server/html"),
		routes: ~[(~"GET", ~"/foo/bar", ~"foo")],
		views: ~[(~"foo",  test_view)],
		load_rsrc: null_loader,
		valid_rsrc: |_path| {true}
		, ..initialize_config()};
		
	let eport = comm::Port();
	let ech = comm::Chan(&eport);
	let iconfig = connection::config_to_conn(&config, ech);
	
	let request = make_request(~"/foo/baz.jpg", ~"text/html,image/jpeg");
	let (header, body) = process_request(&iconfig, &request, ~"10.11.12.13", ~"1.2.3.4");
	
	assert header.contains("Content-Type: image/jpeg");
	match body
	{
		BinaryBody(binary) => assert utils::check_strs(str::from_bytes(*binary), ~"server/html/foo/baz.jpg contents"),
		_ => fail fmt!("Expected binary body but found %?", body),
	}
}

#[test]
fn static_with_bad_type()
{
	let config = Config {
		hosts: ~[~"localhost"],
		server_info: ~"unit test",
		resources_root: path::from_str(~"server/html"),
		routes: ~[(~"GET", ~"/foo/bar", ~"foo")],
		views: ~[(~"foo",  test_view)],
		load_rsrc: null_loader,
		valid_rsrc: |_path| {true}
		, ..initialize_config()};
		
	let eport = comm::Port();
	let ech = comm::Chan(&eport);
	let iconfig = connection::config_to_conn(&config, ech);
	
	let request = make_request(~"/foo/baz.jpg", ~"text/zzz");
	let (header, body) = process_request(&iconfig, &request, ~"10.11.12.13", ~"1.2.3.4");
	
	assert header.contains("Content-Type: text/html");
	assert body.to_str() == ~"server/html/not-found.html contents";
}

#[test]
fn bad_url()
{
	let config = Config {
		hosts: ~[~"localhost"],
		server_info: ~"unit test",
		resources_root: path::from_str(~"server/html"),
		routes: ~[(~"GET", ~"/foo/bar", ~"foo")],
		views: ~[(~"foo",  test_view)],
		load_rsrc: null_loader,
		valid_rsrc: |_path| {false}
		, .. initialize_config()};
		
	let eport = comm::Port();
	let ech = comm::Chan(&eport);
	let iconfig = connection::config_to_conn(&config, ech);
	
	let request = make_request(~"/foo/baz.jpg", ~"text/html,image/jpeg");
	let (header, body) = process_request(&iconfig, &request, ~"10.11.12.13", ~"1.2.3.4");
	
	assert header.contains("Content-Type: text/html");
	assert header.contains("404 Not Found");
	assert str::contains(body.to_str(), "server/html/not-found.html content");
}

#[test]
fn path_outside_root()
{
	let config = Config {
		hosts: ~[~"localhost"],
		server_info: ~"unit test",
		resources_root: path::from_str(~"server/html"),
		routes: ~[(~"GET", ~"/foo/bar", ~"foo")],
		views: ~[(~"foo",  test_view)],
		load_rsrc: null_loader,
		valid_rsrc: |_path| {true}
		, .. initialize_config()};
		
	let eport = comm::Port();
	let ech = comm::Chan(&eport);
	let iconfig = connection::config_to_conn(&config, ech);
	
	let request = make_request(~"/foo/../../baz.jpg", ~"text/html,image/jpeg");
	let (header, body) = process_request(&iconfig, &request, ~"10.11.12.13", ~"1.2.3.4");
	
	assert header.contains("Content-Type: text/html");
	assert header.contains("403 Forbidden");
	assert str::contains(body.to_str(), "server/html/not-found.html contents");
}

#[test]
fn read_error()
{
	let config = Config {
		hosts: ~[~"localhost"],
		server_info: ~"unit test",
		resources_root: path::from_str(~"server/html"),
		routes: ~[(~"GET", ~"/foo/baz", ~"foo")],
		views: ~[(~"foo",  test_view)],
		load_rsrc: err_loader,
		valid_rsrc: |_path| {true}
		, .. initialize_config()};
		
	let eport = comm::Port();
	let ech = comm::Chan(&eport);
	let iconfig = connection::config_to_conn(&config, ech);
	
	let request = make_request(~"/foo/baz.jpg", ~"text/html,image/jpeg");
	let (header, body) = process_request(&iconfig, &request, ~"10.11.12.13", ~"1.2.3.4");
	
	assert header.contains("Content-Type: text/html");
	assert header.contains("403 Forbidden");
	assert str::contains(body.to_str(), "Could not read URL /foo/baz.jpg");
}

#[test]
fn bad_version()
{
	let config = Config {
		hosts: ~[~"localhost"],
		server_info: ~"unit test",
		resources_root: path::from_str(~"server/html"),
		routes: ~[(~"GET", ~"/foo/baz", ~"foo")],
		views: ~[(~"foo",  test_view)],
		load_rsrc: null_loader,
		valid_rsrc: |_path| {true}
		, .. initialize_config()};
		
	let eport = comm::Port();
	let ech = comm::Chan(&eport);
	let iconfig = connection::config_to_conn(&config, ech);
	
	let request = HttpRequest {major_version: 100 , .. make_request(~"/foo/baz.jpg", ~"text/html,image/jpeg")};
	let (header, body) = process_request(&iconfig, &request, ~"10.11.12.13", ~"1.2.3.4");
	
	assert header.contains("Content-Type: text/html");
	assert header.contains("505 HTTP Version Not Supported");
	assert str::contains(body.to_str(), "server/html/not-found.html contents");
}

#[test]
fn bad_template()
{
	fn bad_loader(_path: &Path) ->  result::Result<~[u8], ~str>
	{
		result::Ok(str::to_bytes(~"unbalanced {{curly}} {{braces}"))
	}
	
	let config = Config {
		hosts: ~[~"localhost"],
		server_info: ~"unit test",
		resources_root: path::from_str(~"server/html"),
		routes: ~[(~"GET", ~"/foo/baz", ~"foo")],
		views: ~[(~"foo",  test_view)],
		load_rsrc: bad_loader,
		valid_rsrc: |_path| {true},
		settings: ~[(~"debug", ~"true")],
		..initialize_config()};
		
	let eport = comm::Port();
	let ech = comm::Chan(&eport);
	let iconfig = connection::config_to_conn(&config, ech);
	
	match load_template(&iconfig, &path::from_str(~"blah.html"))
	{
		result::Ok(ref v) =>
		{
			io::stderr().write_line(~"Expected error but found: " + **v);
			assert false;
		}
		result::Err(ref s) =>
		{
			assert str::contains(*s, "mismatched curly braces");
		}
	}
}

#[test]
fn query_strings()
{
	let (path, params) = parse_url(~"/some/url");
	assert utils::check_strs(path, ~"/some/url");
	assert utils::check_vectors(params, ~[]);
	
	let (path, params) = parse_url(~"/some/url?badness");
	assert utils::check_strs(path, ~"/some/url?badness");
	assert utils::check_vectors(params, ~[]);
	
	let (path, params) = parse_url(~"/some?name=value");
	assert utils::check_strs(path, ~"/some");
	assert utils::check_vectors(params, ~[(@~"name", @~"value")]);
	
	let (path, params) = parse_url(~"/some?name=value&foo=bar");
	assert utils::check_strs(path, ~"/some");
	assert utils::check_vectors(params, ~[(@~"name", @~"value"), (@~"foo", @~"bar")]);
}
