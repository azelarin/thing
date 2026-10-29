--!strict

local invalidOriginX: ThingTransformOrigin = {
    x = "center",
}

local invalidOriginOffset: ThingTransformOrigin = {
    offset_y = false,
}

local invalid: ThingStyle = {
    translate_x = true,
    rotation = "clockwise",
    transform_origin = invalidOriginX,
}

local _invalidOriginOffset = invalidOriginOffset

return ui.row { style = invalid }
