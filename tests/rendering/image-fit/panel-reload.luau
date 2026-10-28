local function image(source, fit)
    return ui.image {
        source = ui.asset(source),
        fit = fit,
        style = {
            width = 156,
            height = 100,
            background = '#202020',
        },
    }
end

local function row(extension)
    local source = './pattern.' .. extension
    return ui.row {
        style = { width = 488, height = 100, gap = 10 },
        children = {
            image(source, 'cover'),
            image(source, 'cover'),
            image(source, 'fill'),
        },
    }
end

return ui.panel {
    anchors = { top = true, left = true },
    style = {
        width = 510,
        height = 230,
        padding = 10,
        background = '#181818',
    },
    children = {
        ui.column {
            style = { width = 488, height = 210, gap = 10 },
            children = { row('svg'), row('png') },
        },
    },
}
