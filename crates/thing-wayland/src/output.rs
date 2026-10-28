use smithay_client_toolkit::output::OutputInfo;
use thing_core::Scale;
use thing_runtime::OutputContext;
use wayland_client::protocol::wl_output;

pub(crate) fn output_context(info: OutputInfo) -> Option<OutputContext> {
    let integer_scale = info.scale_factor.clamp(1, 16) as u32;
    let scale = Scale::from_120ths(integer_scale * 120).unwrap_or_default();
    let logical_size = info.logical_size.and_then(valid_dimensions).or_else(|| {
        let dimensions = info
            .modes
            .iter()
            .find(|mode| mode.current)
            .map(|mode| mode.dimensions)?;
        let rotated = matches!(
            info.transform,
            wl_output::Transform::_90
                | wl_output::Transform::_270
                | wl_output::Transform::Flipped90
                | wl_output::Transform::Flipped270
        );
        fallback_logical_size(dimensions, integer_scale, rotated)
    })?;
    Some(OutputContext {
        name: info.name,
        width: logical_size.0,
        height: logical_size.1,
        scale,
    })
}

pub(crate) fn valid_dimensions((width, height): (i32, i32)) -> Option<(u32, u32)> {
    Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?))
        .filter(|(width, height)| *width > 0 && *height > 0)
}

pub(crate) fn fallback_logical_size(
    dimensions: (i32, i32),
    scale: u32,
    rotated: bool,
) -> Option<(u32, u32)> {
    let (mut width, mut height) = valid_dimensions(dimensions)?;
    if rotated {
        std::mem::swap(&mut width, &mut height);
    }
    Some((width / scale, height / scale)).filter(|(width, height)| *width > 0 && *height > 0)
}
