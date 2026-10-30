--!strict

local _radial: ThingLinearGradient = {
    kind = 'radial',
    from = { x = 0, y = 0 },
    to = { x = 1, y = 0 },
    stops = {},
}
local _stop: ThingGradientStop = { position = 'half', color = '#ffffff' }
local _mixed: ThingRadialGradient = {
    kind = 'radial',
    center = { x = 0.5, y = 0.5 },
    radius = { x = 0.5, y = 0.5 },
    from = { x = 0, y = 0 },
    stops = {},
}
local _missing_radius: ThingRadialGradient = {
    kind = 'radial',
    center = { x = 0.5, y = 0.5 },
    stops = {},
}
local _scalar_radius: ThingRadialGradient = {
    kind = 'radial',
    center = { x = 0.5, y = 0.5 },
    radius = 0.5,
    stops = {},
}
return ui.panel {}
