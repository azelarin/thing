--!strict

local width = ui.spring(function()
    return 20
end, { stiffness = 170, damping = 0.8 })

return ui.panel {
    style = { background = width },
}
