/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use cssparser::{Parser as CssParser, ParserInput};
use dom::attr::Attr;
use dom::bindings::cell::DomRefCell;
use dom::bindings::codegen::Bindings::DOMTokenListBinding::DOMTokenListBinding::DOMTokenListMethods;
use dom::bindings::codegen::Bindings::HTMLLinkElementBinding;
use dom::bindings::codegen::Bindings::HTMLLinkElementBinding::HTMLLinkElementMethods;
use dom::bindings::inheritance::Castable;
use dom::bindings::root::{DomRoot, MutNullableDom, RootedReference};
use dom::bindings::str::DOMString;
use dom::cssstylesheet::CSSStyleSheet;
use dom::document::Document;
use dom::domtokenlist::DOMTokenList;
use dom::element::{AttributeMutation, Element, ElementCreator};
use dom::element::{cors_setting_for_element, reflect_cross_origin_attribute, set_cross_origin_attribute};
use dom::htmlelement::HTMLElement;
use dom::node::{Node, UnbindContext, document_from_node, window_from_node};
use dom::stylesheet::StyleSheet as DOMStyleSheet;
use dom::virtualmethods::VirtualMethods;
use dom_struct::dom_struct;
use embedder_traits::EmbedderMsg;
use html5ever::{LocalName, Prefix};
use net_traits::ReferrerPolicy;
use servo_arc::Arc;
use std::borrow::ToOwned;
use std::cell::Cell;
use std::default::Default;
use style::attr::AttrValue;
use style::media_queries::parse_media_query_list;
use style::parser::ParserContext as CssParserContext;
use style::str::HTML_SPACE_CHARACTERS;
use style::stylesheets::{CssRuleType, Stylesheet};
use style_traits::ParsingMode;
use stylesheet_loader::{StylesheetLoader, StylesheetContextSource, StylesheetOwner};

#[derive(Clone, Copy, JSTraceable, MallocSizeOf, PartialEq)]
pub struct RequestGenerationId(u32);

impl RequestGenerationId {
    fn increment(self) -> RequestGenerationId {
        RequestGenerationId(self.0 + 1)
    }
}

#[dom_struct]
pub struct HTMLLinkElement {
    htmlelement: HTMLElement,
    rel_list: MutNullableDom<DOMTokenList>,
    #[ignore_malloc_size_of = "Arc"]
    stylesheet: DomRefCell<Option<Arc<Stylesheet>>>,
    cssom_stylesheet: MutNullableDom<CSSStyleSheet>,

    /// <https://html.spec.whatwg.org/multipage/#a-style-sheet-that-is-blocking-scripts>
    parser_inserted: Cell<bool>,
    /// The number of loads that this link element has triggered (could be more
    /// than one because of imports) and have not yet finished.
    pending_loads: Cell<u32>,
    /// Whether any of the loads have failed.
    any_failed_load: Cell<bool>,
    /// A monotonically increasing counter that keeps track of which stylesheet to apply.
    request_generation_id: Cell<RequestGenerationId>,
}

impl HTMLLinkElement {
    fn new_inherited(local_name: LocalName, prefix: Option<Prefix>, document: &Document,
                     creator: ElementCreator) -> HTMLLinkElement {
        HTMLLinkElement {
            htmlelement: HTMLElement::new_inherited(local_name, prefix, document),
            rel_list: Default::default(),
            parser_inserted: Cell::new(creator.is_parser_created()),
            stylesheet: DomRefCell::new(None),
            cssom_stylesheet: MutNullableDom::new(None),
            pending_loads: Cell::new(0),
            any_failed_load: Cell::new(false),
            request_generation_id: Cell::new(RequestGenerationId(0)),
        }
    }

    #[allow(unrooted_must_root)]
    pub fn new(local_name: LocalName,
               prefix: Option<Prefix>,
               document: &Document,
               creator: ElementCreator) -> DomRoot<HTMLLinkElement> {
        Node::reflect_node(Box::new(HTMLLinkElement::new_inherited(local_name, prefix, document, creator)),
                           document,
                           HTMLLinkElementBinding::Wrap)
    }

    pub fn get_request_generation_id(&self) -> RequestGenerationId {
        self.request_generation_id.get()
    }

    // FIXME(emilio): These methods are duplicated with
    // HTMLStyleElement::set_stylesheet.
    pub fn set_stylesheet(&self, s: Arc<Stylesheet>) {
        let doc = document_from_node(self);
        if let Some(ref s) = *self.stylesheet.borrow() {
            doc.remove_stylesheet(self.upcast(), s)
        }
        *self.stylesheet.borrow_mut() = Some(s.clone());
        self.cssom_stylesheet.set(None);
        doc.add_stylesheet(self.upcast(), s);
    }

    pub fn get_stylesheet(&self) -> Option<Arc<Stylesheet>> {
        self.stylesheet.borrow().clone()
    }

    pub fn get_cssom_stylesheet(&self) -> Option<DomRoot<CSSStyleSheet>> {
        self.get_stylesheet().map(|sheet| {
            self.cssom_stylesheet.or_init(|| {
                CSSStyleSheet::new(&window_from_node(self),
                                   self.upcast::<Element>(),
                                   "text/css".into(),
                                   None, // todo handle location
                                   None, // todo handle title
                                   sheet)
            })
        })
    }

    pub fn is_alternate(&self) -> bool {
        let rel = get_attr(self.upcast(), &local_name!("rel"));
        match rel {
            Some(ref value) => {
                value.split(HTML_SPACE_CHARACTERS)
                    .any(|s| s.eq_ignore_ascii_case("alternate"))
            },
            None => false,
        }
    }
}

fn get_attr(element: &Element, local_name: &LocalName) -> Option<String> {
    let elem = element.get_attribute(&ns!(), local_name);
    elem.map(|e| {
        let value = e.value();
        (**value).to_owned()
    })
}

fn string_is_stylesheet(value: &Option<String>) -> bool {
    match *value {
        Some(ref value) => {
            value.split(HTML_SPACE_CHARACTERS)
                .any(|s| s.eq_ignore_ascii_case("stylesheet"))
        },
        None => false,
    }
}

/// Favicon spec usage in accordance with CEF implementation:
/// only url of icon is required/used
/// <https://html.spec.whatwg.org/multipage/#rel-icon>
fn is_favicon(value: &Option<String>) -> bool {
    match *value {
        Some(ref value) => {
            value.split(HTML_SPACE_CHARACTERS)
                .any(|s| s.eq_ignore_ascii_case("icon") || s.eq_ignore_ascii_case("apple-touch-icon"))
        },
        None => false,
    }
}

impl VirtualMethods for HTMLLinkElement {
    fn super_type(&self) -> Option<&VirtualMethods> {
        Some(self.upcast::<HTMLElement>() as &VirtualMethods)
    }

    fn attribute_mutated(&self, attr: &Attr, mutation: AttributeMutation) {
        self.super_type().unwrap().attribute_mutated(attr, mutation);
        if !self.upcast::<Node>().is_in_doc() || mutation.is_removal() {
            return;
        }

        let rel = get_attr(self.upcast(), &local_name!("rel"));
        match attr.local_name() {
            &local_name!("href") => {
                if string_is_stylesheet(&rel) {
                    self.handle_stylesheet_url(&attr.value());
                } else if is_favicon(&rel) {
                    let sizes = get_attr(self.upcast(), &local_name!("sizes"));
                    self.handle_favicon_url(rel.as_ref().unwrap(), &attr.value(), &sizes);
                }
            },
            &local_name!("sizes") => {
                if is_favicon(&rel) {
                    if let Some(ref href) = get_attr(self.upcast(), &local_name!("href")) {
                        self.handle_favicon_url(rel.as_ref().unwrap(), href, &Some(attr.value().to_string()));
                    }
                }
            },
            _ => {},
        }
    }

    fn parse_plain_attribute(&self, name: &LocalName, value: DOMString) -> AttrValue {
        match name {
            &local_name!("rel") => AttrValue::from_serialized_tokenlist(value.into()),
            _ => self.super_type().unwrap().parse_plain_attribute(name, value),
        }
    }

    fn bind_to_tree(&self, tree_in_doc: bool) {
        if let Some(ref s) = self.super_type() {
            s.bind_to_tree(tree_in_doc);
        }

        if tree_in_doc {
            let element = self.upcast();

            let rel = get_attr(element, &local_name!("rel"));
            let href = get_attr(element, &local_name!("href"));
            let sizes = get_attr(self.upcast(), &local_name!("sizes"));

            match href {
                Some(ref href) if string_is_stylesheet(&rel) => {
                    self.handle_stylesheet_url(href);
                }
                Some(ref href) if is_favicon(&rel) => {
                    self.handle_favicon_url(rel.as_ref().unwrap(), href, &sizes);
                }
                _ => {}
            }
        }
    }

    fn unbind_from_tree(&self, context: &UnbindContext) {
        if let Some(ref s) = self.super_type() {
            s.unbind_from_tree(context);
        }

        if let Some(s) = self.stylesheet.borrow_mut().take() {
            document_from_node(self).remove_stylesheet(self.upcast(), &s);
        }
    }
}


impl HTMLLinkElement {
    /// <https://html.spec.whatwg.org/multipage/#concept-link-obtain>
    fn handle_stylesheet_url(&self, href: &str) {
        let document = document_from_node(self);
        if document.browsing_context().is_none() {
            return;
        }

        // Step 1.
        if href.is_empty() {
            return;
        }

        // Step 2.
        let link_url = match document.base_url().join(href) {
            Ok(url) => url,
            Err(e) => {
                debug!("Parsing url {} failed: {}", href, e);
                return;
            }
        };

        let element = self.upcast::<Element>();

        // Step 3
        let cors_setting = cors_setting_for_element(element);

        let mq_attribute = element.get_attribute(&ns!(), &local_name!("media"));
        let value = mq_attribute.r().map(|a| a.value());
        let mq_str = match value {
            Some(ref value) => &***value,
            None => "",
        };

        let mut input = ParserInput::new(&mq_str);
        let mut css_parser = CssParser::new(&mut input);
        let doc_url = document.url();
        let context = CssParserContext::new_for_cssom(&doc_url, Some(CssRuleType::Media),
                                                      ParsingMode::DEFAULT,
                                                      document.quirks_mode());
        let window = document.window();
        let media = parse_media_query_list(&context, &mut css_parser,
                                           window.css_error_reporter());

        let im_attribute = element.get_attribute(&ns!(), &local_name!("integrity"));
        let integrity_val = im_attribute.r().map(|a| a.value());
        let integrity_metadata = match integrity_val {
            Some(ref value) => &***value,
            None => "",
        };

        self.request_generation_id.set(self.request_generation_id.get().increment());

        // TODO: #8085 - Don't load external stylesheets if the node's mq
        // doesn't match.
        let loader = StylesheetLoader::for_element(self.upcast());
        loader.load(StylesheetContextSource::LinkElement {
            media: Some(media),
        }, link_url, cors_setting, integrity_metadata.to_owned());
    }

    fn handle_favicon_url(&self, _rel: &str, href: &str, _sizes: &Option<String>) {
        let document = document_from_node(self);
        match document.base_url().join(href) {
            Ok(url) => {
                let window = document.window();
                if window.is_top_level() {
                    let msg = EmbedderMsg::NewFavicon(url.clone());
                    window.send_to_embedder(msg);
                }

            }
            Err(e) => debug!("Parsing url {} failed: {}", href, e)
        }
    }
}

impl StylesheetOwner for HTMLLinkElement {
    fn increment_pending_loads_count(&self) {
        self.pending_loads.set(self.pending_loads.get() + 1)
    }

    fn load_finished(&self, succeeded: bool) -> Option<bool> {
        assert!(self.pending_loads.get() > 0, "What finished?");
        if !succeeded {
            self.any_failed_load.set(true);
        }

        self.pending_loads.set(self.pending_loads.get() - 1);
        if self.pending_loads.get() != 0 {
            return None;
        }

        let any_failed = self.any_failed_load.get();
        self.any_failed_load.set(false);
        Some(any_failed)
    }

    fn parser_inserted(&self) -> bool {
        self.parser_inserted.get()
    }

    fn referrer_policy(&self) -> Option<ReferrerPolicy> {
        if self.RelList().Contains("noreferrer".into()) {
            return Some(ReferrerPolicy::NoReferrer)
        }

        None
    }

    fn set_origin_clean(&self, origin_clean: bool) {
        if let Some(stylesheet) = self.get_cssom_stylesheet() {
            stylesheet.set_origin_clean(origin_clean);
        }
    }
}

impl HTMLLinkElementMethods for HTMLLinkElement {
    // https://html.spec.whatwg.org/multipage/#dom-link-href
    make_url_getter!(Href, "href");

    // https://html.spec.whatwg.org/multipage/#dom-link-href
    make_setter!(SetHref, "href");

    // https://html.spec.whatwg.org/multipage/#dom-link-rel
    make_getter!(Rel, "rel");

    // https://html.spec.whatwg.org/multipage/#dom-link-rel
    fn SetRel(&self, rel: DOMString) {
        self.upcast::<Element>().set_tokenlist_attribute(&local_name!("rel"), rel);
    }

    // https://html.spec.whatwg.org/multipage/#dom-link-media
    make_getter!(Media, "media");

    // https://html.spec.whatwg.org/multipage/#dom-link-media
    make_setter!(SetMedia, "media");

    // https://html.spec.whatwg.org/multipage/#dom-link-integrity
    make_getter!(Integrity, "integrity");

    // https://html.spec.whatwg.org/multipage/#dom-link-integrity
    make_setter!(SetIntegrity, "integrity");

    // https://html.spec.whatwg.org/multipage/#dom-link-hreflang
    make_getter!(Hreflang, "hreflang");

    // https://html.spec.whatwg.org/multipage/#dom-link-hreflang
    make_setter!(SetHreflang, "hreflang");

    // https://html.spec.whatwg.org/multipage/#dom-link-type
    make_getter!(Type, "type");

    // https://html.spec.whatwg.org/multipage/#dom-link-type
    make_setter!(SetType, "type");

    // https://html.spec.whatwg.org/multipage/#dom-link-rellist
    fn RelList(&self) -> DomRoot<DOMTokenList> {
        self.rel_list.or_init(|| DOMTokenList::new(self.upcast(), &local_name!("rel")))
    }

    // https://html.spec.whatwg.org/multipage/#dom-link-charset
    make_getter!(Charset, "charset");

    // https://html.spec.whatwg.org/multipage/#dom-link-charset
    make_setter!(SetCharset, "charset");

    // https://html.spec.whatwg.org/multipage/#dom-link-rev
    make_getter!(Rev, "rev");

    // https://html.spec.whatwg.org/multipage/#dom-link-rev
    make_setter!(SetRev, "rev");

    // https://html.spec.whatwg.org/multipage/#dom-link-target
    make_getter!(Target, "target");

    // https://html.spec.whatwg.org/multipage/#dom-link-target
    make_setter!(SetTarget, "target");

    // https://html.spec.whatwg.org/multipage/#dom-link-crossorigin
    fn GetCrossOrigin(&self) -> Option<DOMString> {
        reflect_cross_origin_attribute(self.upcast::<Element>())
    }

    // https://html.spec.whatwg.org/multipage/#dom-link-crossorigin
    fn SetCrossOrigin(&self, value: Option<DOMString>) {
        set_cross_origin_attribute(self.upcast::<Element>(), value);
    }

    // https://drafts.csswg.org/cssom/#dom-linkstyle-sheet
    fn GetSheet(&self) -> Option<DomRoot<DOMStyleSheet>> {
        self.get_cssom_stylesheet().map(DomRoot::upcast)
    }
}
