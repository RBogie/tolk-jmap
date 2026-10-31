//! EAS 16.0 and 16.1 differences, translated at the edge so the command
//! handlers keep working with the 14.1 shapes.
//!
//! - A location is `airsyncbase:Location` with a `DisplayName` (and
//!   optionally an address) instead of a `Location` string, in calendar
//!   items and in meeting requests.
//! - Exceptions name the occurrence they change with `airsyncbase:InstanceId`
//!   instead of `calendar:ExceptionStartTime`.
//!
//! Changes to one occurrence sent as their own Change or Delete (with an
//! `InstanceId` next to the ServerId) and drafts are handled by the
//! calendar and mail Sync code itself.

use crate::wbxml::{
    Element, Node, Tag,
    cp::{airsyncbase as base, calendar as cal, email},
};

/// The first EAS version with these differences.
pub const VERSION: u16 = 160;

/// Rewrites a 16.x request body into the 14.1 shapes the handlers read.
pub fn incoming(root: &mut Element) {
    rewrite(root, &|element, parent| {
        if element.tag == base::Location && parent.is_some_and(holds_calendar_fields) {
            let name = element
                .child_text(base::DisplayName)
                .unwrap_or_default()
                .to_owned();
            *element = Element::with_text(cal::Location, name);
        } else if element.tag == base::InstanceId && parent == Some(cal::Exception) {
            element.tag = cal::ExceptionStartTime;
        }
    });
}

/// Rewrites a response into the 16.x shapes.
pub fn outgoing(root: &mut Element) {
    rewrite(root, &|element, parent| {
        let is_location = element.tag == cal::Location
            || (element.tag == email::Location && parent == Some(email::MeetingRequest));
        if is_location {
            let name = element.text().unwrap_or_default().to_owned();
            *element =
                Element::new(base::Location).child(Element::with_text(base::DisplayName, name));
        } else if element.tag == cal::ExceptionStartTime {
            element.tag = base::InstanceId;
        }
    });
}

/// Where a calendar location can appear: an item's data or an exception.
fn holds_calendar_fields(tag: Tag) -> bool {
    tag == crate::wbxml::cp::airsync::ApplicationData || tag == cal::Exception
}

/// Calls `f` on every element below `root` with its parent's tag.
fn rewrite(root: &mut Element, f: &dyn Fn(&mut Element, Option<Tag>)) {
    let parent = root.tag;
    for node in &mut root.children {
        if let Node::Element(child) = node {
            f(child, Some(parent));
            rewrite(child, f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wbxml::cp::airsync;

    fn event(location: Element, exception_time: Element) -> Element {
        Element::new(airsync::ApplicationData)
            .child(Element::with_text(cal::Subject, "Gym"))
            .child(location)
            .child(
                Element::new(cal::Exceptions).child(
                    Element::new(cal::Exception)
                        .child(exception_time)
                        .child(Element::with_text(cal::Deleted, "1")),
                ),
            )
    }

    #[test]
    fn translates_locations_and_exceptions_both_ways() {
        let v14 = event(
            Element::with_text(cal::Location, "Room 1"),
            Element::with_text(cal::ExceptionStartTime, "20261105T170000Z"),
        );
        let v16 = event(
            Element::new(base::Location).child(Element::with_text(base::DisplayName, "Room 1")),
            Element::with_text(base::InstanceId, "20261105T170000Z"),
        );
        let mut out = v14.clone();
        outgoing(&mut out);
        assert_eq!(out, v16);
        let mut back = v16;
        incoming(&mut back);
        assert_eq!(back, v14);
    }

    #[test]
    fn meeting_request_locations_too() {
        let mut data = Element::new(airsync::ApplicationData).child(
            Element::new(email::MeetingRequest).child(Element::with_text(email::Location, "Cafe")),
        );
        outgoing(&mut data);
        let location = data
            .find(email::MeetingRequest)
            .and_then(|m| m.find(base::Location))
            .and_then(|l| l.child_text(base::DisplayName));
        assert_eq!(location, Some("Cafe"));
    }
}
