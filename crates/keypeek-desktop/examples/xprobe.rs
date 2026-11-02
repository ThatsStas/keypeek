//! Ad-hoc probe: print map state / geometry / override-redirect for window ids.
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;

fn main() {
    let (c, _) = XCBConnection::connect(None).expect("connect");
    for arg in std::env::args().skip(1) {
        let xid: u32 = arg.parse().expect("xid");
        match c
            .get_window_attributes(xid)
            .ok()
            .and_then(|r| r.reply().ok())
        {
            Some(a) => println!(
                "{xid} map_state={:?} override_redirect={} visual={:#x}",
                a.map_state, a.override_redirect, a.visual
            ),
            None => {
                println!("{xid} attributes: error");
                continue;
            }
        }
        if let Some(g) = c.get_geometry(xid).ok().and_then(|r| r.reply().ok()) {
            println!("    geom {}x{} @ {} {}", g.width, g.height, g.x, g.y);
        }
        use x11rb::protocol::shape::ConnectionExt as _;
        match c
            .shape_get_rectangles(xid, x11rb::protocol::shape::SK::INPUT)
            .ok()
            .and_then(|r| r.reply().ok())
        {
            Some(r) => println!(
                "    input shape: {} rectangles",
                r.rectangles.len()
            ),
            None => println!("    input shape: query failed"),
        }
    }
}
