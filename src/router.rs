use std::cmp::min;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use regex::{escape, Regex};
use url::Url;

use error::UrlGenerationError;
use handler::{AsyncResult, FromRequest, Responder, RouteHandler};
use http::{Method, StatusCode};
use httprequest::HttpRequest;
use httpresponse::HttpResponse;
use param::{ParamItem, Params};
use pred::Predicate;
use resource::{DefaultResource, Resource};
use scope::Scope;
use server::Request;

#[derive(Debug, Copy, Clone, PartialEq)]
pub(crate) enum ResourceId {
    Default,
    Normal(u16),
}

enum ResourcePattern<S> {
    Resource(ResourceDef),
    Handler(ResourceDef, Option<Vec<Box<Predicate<S>>>>),
    Scope(ResourceDef, Vec<Box<Predicate<S>>>),
}

enum ResourceItem<S> {
    Resource(Resource<S>),
    Handler(Box<RouteHandler<S>>),
    Scope(Scope<S>),
}

/// Interface for application router.
pub struct Router<S> {
    defs: Rc<Inner>,
    patterns: Vec<ResourcePattern<S>>,
    resources: Vec<ResourceItem<S>>,
    default: Option<DefaultResource<S>>,
}

/// Information about current resource
#[derive(Clone)]
pub struct ResourceInfo {
    router: Rc<Inner>,
    resource: ResourceId,
    params: Params,
    prefix: u16,
}

impl ResourceInfo {
    /// Name os the resource
    #[inline]
    pub fn name(&self) -> &str {
        if let ResourceId::Normal(idx) = self.resource {
            self.router.patterns[idx as usize].name()
        } else {
            ""
        }
    }

    /// This method returns reference to matched `ResourceDef` object.
    #[inline]
    pub fn rdef(&self) -> Option<&ResourceDef> {
        if let ResourceId::Normal(idx) = self.resource {
            Some(&self.router.patterns[idx as usize])
        } else {
            None
        }
    }

    pub(crate) fn set_prefix(&mut self, prefix: u16) {
        self.prefix = prefix;
    }

    /// Get a reference to the Params object.
    ///
    /// Params is a container for url parameters.
    /// A variable segment is specified in the form `{identifier}`,
    /// where the identifier can be used later in a request handler to
    /// access the matched value for that segment.
    #[inline]
    pub fn match_info(&self) -> &Params {
        &self.params
    }

    #[inline]
    pub(crate) fn merge(&mut self, info: &ResourceInfo) {
        let mut p = info.params.clone();
        p.set_tail(self.params.tail);
        for item in &self.params.segments {
            p.add(item.0.clone(), item.1.clone());
        }

        self.prefix = info.params.tail;
        self.params = p;
    }

    /// Generate url for named resource
    ///
    /// Check [`HttpRequest::url_for()`](../struct.HttpRequest.html#method.
    /// url_for) for detailed information.
    pub fn url_for<U, I>(
        &self, req: &Request, name: &str, elements: U,
    ) -> Result<Url, UrlGenerationError>
    where
        U: IntoIterator<Item = I>,
        I: AsRef<str>,
    {
        if let Some(pattern) = self.router.named.get(name) {
            let path =
                pattern.resource_path(elements, &req.path()[..(self.prefix as usize)])?;
            if path.starts_with('/') {
                let conn = req.connection_info();
                Ok(Url::parse(&format!(
                    "{}://{}{}",
                    conn.scheme(),
                    conn.host(),
                    path
                ))?)
            } else {
                Ok(Url::parse(&path)?)
            }
        } else {
            Err(UrlGenerationError::ResourceNotFound)
        }
    }

    /// Check if application contains matching route.
    ///
    /// This method does not take `prefix` into account.
    /// For example if prefix is `/test` and router contains route `/name`,
    /// following path would be recognizable `/test/name` but `has_route()` call
    /// would return `false`.
    pub fn has_route(&self, path: &str) -> bool {
        let path = if path.is_empty() { "/" } else { path };

        for pattern in &self.router.patterns {
            if pattern.is_match(path) {
                return true;
            }
        }
        false
    }
}

struct Inner {
    named: HashMap<String, ResourceDef>,
    patterns: Vec<ResourceDef>,
}

impl<S: 'static> Default for Router<S> {
    fn default() -> Self {
        Router::new()
    }
}

impl<S: 'static> Router<S> {
    pub(crate) fn new() -> Self {
        Router {
            defs: Rc::new(Inner {
                named: HashMap::new(),
                patterns: Vec::new(),
            }),
            resources: Vec::new(),
            patterns: Vec::new(),
            default: None,
        }
    }

    #[inline]
    pub(crate) fn route_info_params(&self, idx: u16, params: Params) -> ResourceInfo {
        ResourceInfo {
            params,
            prefix: 0,
            router: self.defs.clone(),
            resource: ResourceId::Normal(idx),
        }
    }

    #[cfg(test)]
    pub(crate) fn route_info(&self, req: &Request, prefix: u16) -> ResourceInfo {
        let mut params = Params::with_url(req.url());
        params.set_tail(prefix);

        ResourceInfo {
            params,
            prefix: 0,
            router: self.defs.clone(),
            resource: ResourceId::Default,
        }
    }

    #[cfg(test)]
    pub(crate) fn default_route_info(&self) -> ResourceInfo {
        ResourceInfo {
            params: Params::new(),
            router: self.defs.clone(),
            resource: ResourceId::Default,
            prefix: 0,
        }
    }

    pub(crate) fn register_resource(&mut self, resource: Resource<S>) {
        {
            let inner = Rc::get_mut(&mut self.defs).unwrap();

            let name = resource.get_name();
            if !name.is_empty() {
                assert!(
                    !inner.named.contains_key(name),
                    "Named resource {:?} is registered.",
                    name
                );
                inner.named.insert(name.to_owned(), resource.rdef().clone());
            }
            inner.patterns.push(resource.rdef().clone());
        }
        self.patterns
            .push(ResourcePattern::Resource(resource.rdef().clone()));
        self.resources.push(ResourceItem::Resource(resource));
    }

    pub(crate) fn register_scope(&mut self, mut scope: Scope<S>) {
        Rc::get_mut(&mut self.defs)
            .unwrap()
            .patterns
            .push(scope.rdef().clone());
        let filters = scope.take_filters();
        self.patterns
            .push(ResourcePattern::Scope(scope.rdef().clone(), filters));
        self.resources.push(ResourceItem::Scope(scope));
    }

    pub(crate) fn register_handler(
        &mut self, path: &str, hnd: Box<RouteHandler<S>>,
        filters: Option<Vec<Box<Predicate<S>>>>,
    ) {
        let rdef = ResourceDef::prefix(path);
        Rc::get_mut(&mut self.defs)
            .unwrap()
            .patterns
            .push(rdef.clone());
        self.resources.push(ResourceItem::Handler(hnd));
        self.patterns.push(ResourcePattern::Handler(rdef, filters));
    }

    pub(crate) fn has_default_resource(&self) -> bool {
        self.default.is_some()
    }

    pub(crate) fn register_default_resource(&mut self, resource: DefaultResource<S>) {
        self.default = Some(resource);
    }

    pub(crate) fn finish(&mut self) {
        if let Some(ref default) = self.default {
            for resource in &mut self.resources {
                match resource {
                    ResourceItem::Resource(_) => (),
                    ResourceItem::Scope(scope) => {
                        if !scope.has_default_resource() {
                            scope.default_resource(default.clone());
                        }
                        scope.finish()
                    }
                    ResourceItem::Handler(hnd) => {
                        if !hnd.has_default_resource() {
                            hnd.default_resource(default.clone());
                        }
                        hnd.finish()
                    }
                }
            }
        }
    }

    pub(crate) fn register_external(&mut self, name: &str, rdef: ResourceDef) {
        let inner = Rc::get_mut(&mut self.defs).unwrap();
        assert!(
            !inner.named.contains_key(name),
            "Named resource {:?} is registered.",
            name
        );
        inner.named.insert(name.to_owned(), rdef);
    }

    pub(crate) fn register_route<T, F, R>(&mut self, path: &str, method: Method, f: F)
    where
        F: Fn(T) -> R + 'static,
        R: Responder + 'static,
        T: FromRequest<S> + 'static,
    {
        let out = {
            // get resource handler
            let mut iterator = self.resources.iter_mut();

            loop {
                if let Some(ref mut resource) = iterator.next() {
                    if let ResourceItem::Resource(ref mut resource) = resource {
                        if resource.rdef().pattern() == path {
                            resource.method(method).with(f);
                            break None;
                        }
                    }
                } else {
                    let mut resource = Resource::new(ResourceDef::new(path));
                    resource.method(method).with(f);
                    break Some(resource);
                }
            }
        };
        if let Some(out) = out {
            self.register_resource(out);
        }
    }

    /// Handle request
    pub fn handle(&self, req: &HttpRequest<S>) -> AsyncResult<HttpResponse> {
        let resource = match req.resource().resource {
            ResourceId::Normal(idx) => &self.resources[idx as usize],
            ResourceId::Default => {
                if let Some(ref default) = self.default {
                    if let Some(id) = default.get_route_id(req) {
                        return default.handle(id, req);
                    }
                }
                return AsyncResult::ok(HttpResponse::new(StatusCode::NOT_FOUND));
            }
        };
        match resource {
            ResourceItem::Resource(ref resource) => {
                if let Some(id) = resource.get_route_id(req) {
                    return resource.handle(id, req);
                }

                if let Some(ref default) = self.default {
                    if let Some(id) = default.get_route_id(req) {
                        return default.handle(id, req);
                    }
                }
            }
            ResourceItem::Handler(hnd) => return hnd.handle(req),
            ResourceItem::Scope(hnd) => return hnd.handle(req),
        }
        AsyncResult::ok(HttpResponse::new(StatusCode::NOT_FOUND))
    }

    /// Query for matched resource
    pub fn recognize(&self, req: &Request, state: &S, tail: usize) -> ResourceInfo {
        if tail <= req.path().len() {
            'outer: for (idx, resource) in self.patterns.iter().enumerate() {
                match resource {
                    ResourcePattern::Resource(rdef) => {
                        if let Some(params) = rdef.match_with_params(req, tail) {
                            return self.route_info_params(idx as u16, params);
                        }
                    }
                    ResourcePattern::Handler(rdef, filters) => {
                        if let Some(params) = rdef.match_prefix_with_params(req, tail) {
                            if let Some(ref filters) = filters {
                                for filter in filters {
                                    if !filter.check(req, state) {
                                        continue 'outer;
                                    }
                                }
                            }
                            return self.route_info_params(idx as u16, params);
                        }
                    }
                    ResourcePattern::Scope(rdef, filters) => {
                        if let Some(params) = rdef.match_prefix_with_params(req, tail) {
                            for filter in filters {
                                if !filter.check(req, state) {
                                    continue 'outer;
                                }
                            }
                            return self.route_info_params(idx as u16, params);
                        }
                    }
                }
            }
        }
        ResourceInfo {
            prefix: tail as u16,
            params: Params::new(),
            router: self.defs.clone(),
            resource: ResourceId::Default,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum PatternElement {
    Str(String),
    Var(String),
}

#[derive(Clone, Debug)]
enum PatternType {
    Static(String),
    Prefix(String),
    Dynamic(Regex, Vec<Rc<String>>, usize),
}

#[derive(Debug, Copy, Clone, PartialEq)]
/// Resource type
pub enum ResourceType {
    /// Normal resource
    Normal,
    /// Resource for application default handler
    Default,
    /// External resource
    External,
    /// Unknown resource type
    Unset,
}

/// Resource type describes an entry in resources table
#[derive(Clone, Debug)]
pub struct ResourceDef {
    tp: PatternType,
    rtp: ResourceType,
    name: String,
    pattern: String,
    elements: Vec<PatternElement>,
}

impl ResourceDef {
    /// Parse path pattern and create new `Resource` instance.
    ///
    /// Panics if path pattern is wrong.
    pub fn new(path: &str) -> Self {
        ResourceDef::with_prefix(path, "/", false)
    }

    /// Parse path pattern and create new `Resource` instance.
    ///
    /// Use `prefix` type instead of `static`.
    ///
    /// Panics if path regex pattern is wrong.
    pub fn prefix(path: &str) -> Self {
        ResourceDef::with_prefix(path, "/", true)
    }

    /// Construct external resource
    ///
    /// Panics if path pattern is wrong.
    pub fn external(path: &str) -> Self {
        let mut resource = ResourceDef::with_prefix(path, "/", false);
        resource.rtp = ResourceType::External;
        resource
    }

    /// Parse path pattern and create new `Resource` instance with custom prefix
    pub fn with_prefix(path: &str, prefix: &str, for_prefix: bool) -> Self {
        let (pattern, elements, is_dynamic, len) =
            ResourceDef::parse(path, prefix, for_prefix);

        let tp = if is_dynamic {
            let re = match Regex::new(&pattern) {
                Ok(re) => re,
                Err(err) => panic!("Wrong path pattern: \"{}\" {}", path, err),
            };
            // actix creates one router per thread
            let names = re
                .capture_names()
                .filter_map(|name| name.map(|name| Rc::new(name.to_owned())))
                .collect();
            PatternType::Dynamic(re, names, len)
        } else if for_prefix {
            PatternType::Prefix(pattern.clone())
        } else {
            PatternType::Static(pattern.clone())
        };

        ResourceDef {
            tp,
            elements,
            name: "".to_string(),
            rtp: ResourceType::Normal,
            pattern: path.to_owned(),
        }
    }

    /// Resource type
    pub fn rtype(&self) -> ResourceType {
        self.rtp
    }

    /// Resource name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Resource name
    pub(crate) fn set_name(&mut self, name: &str) {
        self.name = name.to_owned();
    }

    /// Path pattern of the resource
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Is this path a match against this resource?
    pub fn is_match(&self, path: &str) -> bool {
        match self.tp {
            PatternType::Static(ref s) => s == path,
            PatternType::Dynamic(ref re, _, _) => re.is_match(path),
            PatternType::Prefix(ref s) => path.starts_with(s),
        }
    }

    /// Are the given path and parameters a match against this resource?
    pub fn match_with_params(&self, req: &Request, plen: usize) -> Option<Params> {
        let path = &req.path()[plen..];

        match self.tp {
            PatternType::Static(ref s) => if s != path {
                None
            } else {
                Some(Params::with_url(req.url()))
            },
            PatternType::Dynamic(ref re, ref names, _) => {
                if let Some(captures) = re.captures(path) {
                    let mut params = Params::with_url(req.url());
                    let mut idx = 0;
                    let mut passed = false;
                    for capture in captures.iter() {
                        if let Some(ref m) = capture {
                            if !passed {
                                passed = true;
                                continue;
                            }
                            params.add(
                                names[idx].clone(),
                                ParamItem::UrlSegment(
                                    (plen + m.start()) as u16,
                                    (plen + m.end()) as u16,
                                ),
                            );
                            idx += 1;
                        }
                    }
                    params.set_tail(req.path().len() as u16);
                    Some(params)
                } else {
                    None
                }
            }
            PatternType::Prefix(ref s) => if !path.starts_with(s) {
                None
            } else {
                Some(Params::with_url(req.url()))
            },
        }
    }

    /// Is the given path a prefix match and do the parameters match against this resource?
    pub fn match_prefix_with_params(
        &self, req: &Request, plen: usize,
    ) -> Option<Params> {
        let path = &req.path()[plen..];
        let path = if path.is_empty() { "/" } else { path };

        match self.tp {
            PatternType::Static(ref s) => if s == path {
                Some(Params::with_url(req.url()))
            } else {
                None
            },
            PatternType::Dynamic(ref re, ref names, len) => {
                if let Some(captures) = re.captures(path) {
                    let mut params = Params::with_url(req.url());
                    let mut pos = 0;
                    let mut passed = false;
                    let mut idx = 0;
                    for capture in captures.iter() {
                        if let Some(ref m) = capture {
                            if !passed {
                                passed = true;
                                continue;
                            }

                            params.add(
                                names[idx].clone(),
                                ParamItem::UrlSegment(
                                    (plen + m.start()) as u16,
                                    (plen + m.end()) as u16,
                                ),
                            );
                            idx += 1;
                            pos = m.end();
                        }
                    }
                    params.set_tail((plen + pos + len) as u16);
                    Some(params)
                } else {
                    None
                }
            }
            PatternType::Prefix(ref s) => {
                let len = if path == s {
                    s.len()
                } else if path.starts_with(s)
                    && (s.ends_with('/') || path.split_at(s.len()).1.starts_with('/'))
                {
                    if s.ends_with('/') {
                        s.len() - 1
                    } else {
                        s.len()
                    }
                } else {
                    return None;
                };
                let mut params = Params::with_url(req.url());
                params.set_tail(min(req.path().len(), plen + len) as u16);
                Some(params)
            }
        }
    }

    /// Build resource path.
    pub fn resource_path<U, I>(
        &self, elements: U, prefix: &str,
    ) -> Result<String, UrlGenerationError>
    where
        U: IntoIterator<Item = I>,
        I: AsRef<str>,
    {
        let mut path = match self.tp {
            PatternType::Prefix(ref p) => p.to_owned(),
            PatternType::Static(ref p) => p.to_owned(),
            PatternType::Dynamic(..) => {
                let mut path = String::new();
                let mut iter = elements.into_iter();
                for el in &self.elements {
                    match *el {
                        PatternElement::Str(ref s) => path.push_str(s),
                        PatternElement::Var(_) => {
                            if let Some(val) = iter.next() {
                                path.push_str(val.as_ref())
                            } else {
                                return Err(UrlGenerationError::NotEnoughElements);
                            }
                        }
                    }
                }
                path
            }
        };

        if self.rtp != ResourceType::External {
            if prefix.ends_with('/') {
                if path.starts_with('/') {
                    path.insert_str(0, &prefix[..prefix.len() - 1]);
                } else {
                    path.insert_str(0, prefix);
                }
            } else {
                if !path.starts_with('/') {
                    path.insert(0, '/');
                }
                path.insert_str(0, prefix);
            }
        }
        Ok(path)
    }

    fn parse(
        pattern: &str, prefix: &str, for_prefix: bool,
    ) -> (String, Vec<PatternElement>, bool, usize) {
        const DEFAULT_PATTERN: &str = "[^/]+";

        let mut re1 = String::from("^") + prefix;
        let mut re2 = String::from(prefix);
        let mut el = String::new();
        let mut in_param = false;
        let mut in_param_pattern = false;
        let mut param_name = String::new();
        let mut param_pattern = String::from(DEFAULT_PATTERN);
        let mut is_dynamic = false;
        let mut elems = Vec::new();
        let mut len = 0;

        for (index, ch) in pattern.chars().enumerate() {
            // All routes must have a leading slash so its optional to have one
            if index == 0 && ch == '/' {
                continue;
            }

            if in_param {
                // In parameter segment: `{....}`
                if ch == '}' {
                    elems.push(PatternElement::Var(param_name.clone()));
                    re1.push_str(&format!(r"(?P<{}>{})", &param_name, &param_pattern));

                    param_name.clear();
                    param_pattern = String::from(DEFAULT_PATTERN);

                    len = 0;
                    in_param_pattern = false;
                    in_param = false;
                } else if ch == ':' {
                    // The parameter name has been determined; custom pattern land
                    in_param_pattern = true;
                    param_pattern.clear();
                } else if in_param_pattern {
                    // Ignore leading whitespace for pattern
                    if !(ch == ' ' && param_pattern.is_empty()) {
                        param_pattern.push(ch);
                    }
                } else {
                    param_name.push(ch);
                }
            } else if ch == '{' {
                in_param = true;
                is_dynamic = true;
                elems.push(PatternElement::Str(el.clone()));
                el.clear();
            } else {
                re1.push_str(escape(&ch.to_string()).as_str());
                re2.push(ch);
                el.push(ch);
                len += 1;
            }
        }

        if !el.is_empty() {
            elems.push(PatternElement::Str(el.clone()));
        }

        let re = if is_dynamic {
            if !for_prefix {
                re1.push('$');
            }
            re1
        } else {
            re2
        };
        (re, elems, is_dynamic, len)
    }
}

impl PartialEq for ResourceDef {
    fn eq(&self, other: &ResourceDef) -> bool {
        self.pattern == other.pattern
    }
}

impl Eq for ResourceDef {}

impl Hash for ResourceDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.pattern.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test::TestRequest;

    #[test]
    fn test_recognizer10() {
        let mut router = Router::<()>::new();
        router.register_resource(Resource::new(ResourceDef::new("/name")));
        router.register_resource(Resource::new(ResourceDef::new("/name/{val}")));
        router.register_resource(Resource::new(ResourceDef::new(
            "/name/{val}/index.html",
        )));
        router.register_resource(Resource::new(ResourceDef::new("/file/{file}.{ext}")));
        router.register_resource(Resource::new(ResourceDef::new(
            "/v{val}/{val2}/index.html",
        )));
        router.register_resource(Resource::new(ResourceDef::new("/v/{tail:.*}")));
        router.register_resource(Resource::new(ResourceDef::new("/test2/{test}.html")));
        router.register_resource(Resource::new(ResourceDef::new("{test}/index.html")));

        let req = TestRequest::with_uri("/name").finish();
        let info = router.recognize(&req, &(), 0);
        assert_eq!(info.resource, ResourceId::Normal(0));
        assert!(info.match_info().is_empty());

        let req = TestRequest::with_uri("/name/value").finish();
        let info = router.recognize(&req, &(), 0);
        assert_eq!(info.resource, ResourceId::Normal(1));
        assert_eq!(info.match_info().get("val").unwrap(), "value");
        assert_eq!(&info.match_info()["val"], "value");

        let req = TestRequest::with_uri("/name/value2/index.html").finish();
        let info = router.recognize(&req, &(), 0);
        assert_eq!(info.resource, ResourceId::Normal(2));
        assert_eq!(info.match_info().get("val").unwrap(), "value2");

        let req = TestRequest::with_uri("/file/file.gz").finish();
        let info = router.recognize(&req, &(), 0);
        assert_eq!(info.resource, ResourceId::Normal(3));
        assert_eq!(info.match_info().get("file").unwrap(), "file");
        assert_eq!(info.match_info().get("ext").unwrap(), "gz");

        let req = TestRequest::with_uri("/vtest/ttt/index.html").finish();
        let info = router.recognize(&req, &(), 0);
        assert_eq!(info.resource, ResourceId::Normal(4));
        assert_eq!(info.match_info().get("val").unwrap(), "test");
        assert_eq!(info.match_info().get("val2").unwrap(), "ttt");

        let req = TestRequest::with_uri("/v/blah-blah/index.html").finish();
        let info = router.recognize(&req, &(), 0);
        assert_eq!(info.resource, ResourceId::Normal(5));
        assert_eq!(
            info.match_info().get("tail").unwrap(),
            "blah-blah/index.html"
        );

        let req = TestRequest::with_uri("/test2/index.html").finish();
        let info = router.recognize(&req, &(), 0);
        assert_eq!(info.resource, ResourceId::Normal(6));
        assert_eq!(info.match_info().get("test").unwrap(), "index");

        let req = TestRequest::with_uri("/bbb/index.html").finish();
        let info = router.recognize(&req, &(), 0);
        assert_eq!(info.resource, ResourceId::Normal(7));
        assert_eq!(info.match_info().get("test").unwrap(), "bbb");
    }

    #[test]
    fn test_recognizer_2() {
        let mut router = Router::<()>::new();
        router.register_resource(Resource::new(ResourceDef::new("/index.json")));
        router.register_resource(Resource::new(ResourceDef::new("/{source}.json")));

        let req = TestRequest::with_uri("/index.json").finish();
        let info = router.recognize(&req, &(), 0);
        assert_eq!(info.resource, ResourceId::Normal(0));

        let req = TestRequest::with_uri("/test.json").finish();
        let info = router.recognize(&req, &(), 0);
        assert_eq!(info.resource, ResourceId::Normal(1));
    }

    #[test]
    fn test_recognizer_with_prefix() {
        let mut router = Router::<()>::new();
        router.register_resource(Resource::new(ResourceDef::new("/name")));
        router.register_resource(Resource::new(ResourceDef::new("/name/{val}")));

        let req = TestRequest::with_uri("/name").finish();
        let info = router.recognize(&req, &(), 5);
        assert_eq!(info.resource, ResourceId::Default);

        let req = TestRequest::with_uri("/test/name").finish();
        let info = router.recognize(&req, &(), 5);
        assert_eq!(info.resource, ResourceId::Normal(0));

        let req = TestRequest::with_uri("/test/name/value").finish();
        let info = router.recognize(&req, &(), 5);
        assert_eq!(info.resource, ResourceId::Normal(1));
        assert_eq!(info.match_info().get("val").unwrap(), "value");
        assert_eq!(&info.match_info()["val"], "value");

        // same patterns
        let mut router = Router::<()>::new();
        router.register_resource(Resource::new(ResourceDef::new("/name")));
        router.register_resource(Resource::new(ResourceDef::new("/name/{val}")));

        let req = TestRequest::with_uri("/name").finish();
        let info = router.recognize(&req, &(), 6);
        assert_eq!(info.resource, ResourceId::Default);

        let req = TestRequest::with_uri("/test2/name").finish();
        let info = router.recognize(&req, &(), 6);
        assert_eq!(info.resource, ResourceId::Normal(0));

        let req = TestRequest::with_uri("/test2/name-test").finish();
        let info = router.recognize(&req, &(), 6);
        assert_eq!(info.resource, ResourceId::Default);

        let req = TestRequest::with_uri("/test2/name/ttt").finish();
        let info = router.recognize(&req, &(), 6);
        assert_eq!(info.resource, ResourceId::Normal(1));
        assert_eq!(&info.match_info()["val"], "ttt");
    }

    #[test]
    fn test_parse_static() {
        let re = ResourceDef::new("/");
        assert!(re.is_match("/"));
        assert!(!re.is_match("/a"));

        let re = ResourceDef::new("/name");
        assert!(re.is_match("/name"));
        assert!(!re.is_match("/name1"));
        assert!(!re.is_match("/name/"));
        assert!(!re.is_match("/name~"));

        let re = ResourceDef::new("/name/");
        assert!(re.is_match("/name/"));
        assert!(!re.is_match("/name"));
        assert!(!re.is_match("/name/gs"));

        let re = ResourceDef::new("/user/profile");
        assert!(re.is_match("/user/profile"));
        assert!(!re.is_match("/user/profile/profile"));
    }

    #[test]
    fn test_parse_param() {
        let re = ResourceDef::new("/user/{id}");
        assert!(re.is_match("/user/profile"));
        assert!(re.is_match("/user/2345"));
        assert!(!re.is_match("/user/2345/"));
        assert!(!re.is_match("/user/2345/sdg"));

        let req = TestRequest::with_uri("/user/profile").finish();
        let info = re.match_with_params(&req, 0).unwrap();
        assert_eq!(info.get("id").unwrap(), "profile");

        let req = TestRequest::with_uri("/user/1245125").finish();
        let info = re.match_with_params(&req, 0).unwrap();
        assert_eq!(info.get("id").unwrap(), "1245125");

        let re = ResourceDef::new("/v{version}/resource/{id}");
        assert!(re.is_match("/v1/resource/320120"));
        assert!(!re.is_match("/v/resource/1"));
        assert!(!re.is_match("/resource"));

        let req = TestRequest::with_uri("/v151/resource/adahg32").finish();
        let info = re.match_with_params(&req, 0).unwrap();
        assert_eq!(info.get("version").unwrap(), "151");
        assert_eq!(info.get("id").unwrap(), "adahg32");
    }

    #[test]
    fn test_resource_prefix() {
        let re = ResourceDef::prefix("/name");
        assert!(re.is_match("/name"));
        assert!(re.is_match("/name/"));
        assert!(re.is_match("/name/test/test"));
        assert!(re.is_match("/name1"));
        assert!(re.is_match("/name~"));

        let re = ResourceDef::prefix("/name/");
        assert!(re.is_match("/name/"));
        assert!(re.is_match("/name/gs"));
        assert!(!re.is_match("/name"));
    }

    #[test]
    fn test_reousrce_prefix_dynamic() {
        let re = ResourceDef::prefix("/{name}/");
        assert!(re.is_match("/name/"));
        assert!(re.is_match("/name/gs"));
        assert!(!re.is_match("/name"));

        let req = TestRequest::with_uri("/test2/").finish();
        let info = re.match_with_params(&req, 0).unwrap();
        assert_eq!(&info["name"], "test2");
        assert_eq!(&info[0], "test2");

        let req = TestRequest::with_uri("/test2/subpath1/subpath2/index.html").finish();
        let info = re.match_with_params(&req, 0).unwrap();
        assert_eq!(&info["name"], "test2");
        assert_eq!(&info[0], "test2");
    }

    #[test]
    fn test_request_resource() {
        let mut router = Router::<()>::new();
        let mut resource = Resource::new(ResourceDef::new("/index.json"));
        resource.name("r1");
        router.register_resource(resource);
        let mut resource = Resource::new(ResourceDef::new("/test.json"));
        resource.name("r2");
        router.register_resource(resource);

        let req = TestRequest::with_uri("/index.json").finish();
        let info = router.recognize(&req, &(), 0);
        assert_eq!(info.resource, ResourceId::Normal(0));

        assert_eq!(info.name(), "r1");

        let req = TestRequest::with_uri("/test.json").finish();
        let info = router.recognize(&req, &(), 0);
        assert_eq!(info.resource, ResourceId::Normal(1));
        assert_eq!(info.name(), "r2");
    }
}
