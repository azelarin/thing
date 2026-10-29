--!strict

local _radial: ThingLinearGradient = {
    kind = 'radial',
    from = { x = 0, y = 0 },
    to = { x = 1, y = 0 },
    stops = {},
}
local _stop: ThingGradientStop = { position = 'half', color = '#ffffff' }
return ui.panel {}
