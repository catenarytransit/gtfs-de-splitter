use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct GeoJson {
    features: Vec<Feature>,
}

#[derive(Debug, Deserialize)]
struct Feature {
    properties: Properties,
    geometry: Geometry,
}

#[derive(Debug, Deserialize)]
struct Properties {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Geometry {
    Polygon {
        coordinates: Vec<Vec<Vec<f64>>>,
    },
    MultiPolygon {
        coordinates: Vec<Vec<Vec<Vec<f64>>>>,
    },
}

impl Geometry {
    fn to_multipolygon(&self) -> Vec<Vec<Vec<Vec<f64>>>> {
        match self {
            Geometry::Polygon { coordinates } => vec![coordinates.clone()],
            Geometry::MultiPolygon { coordinates } => coordinates.clone(),
        }
    }
}

struct StateBoundary {
    name: String,
    clean_name: String,
    coordinates: Vec<Vec<Vec<Vec<f64>>>>,
    bbox: (f64, f64, f64, f64),
}

fn transliterate_state_name(name: &str) -> String {
    name.to_lowercase()
        .replace("ä", "ae")
        .replace("ö", "oe")
        .replace("ü", "ue")
        .replace("ß", "ss")
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

fn calculate_bbox(coords: &[Vec<Vec<Vec<f64>>>]) -> (f64, f64, f64, f64) {
    let mut min_lon = f64::INFINITY;
    let mut max_lon = f64::NEG_INFINITY;
    let mut min_lat = f64::INFINITY;
    let mut max_lat = f64::NEG_INFINITY;

    for poly in coords {
        for ring in poly {
            for pt in ring {
                if pt.len() >= 2 {
                    let lon = pt[0];
                    let lat = pt[1];
                    if lon < min_lon { min_lon = lon; }
                    if lon > max_lon { max_lon = lon; }
                    if lat < min_lat { min_lat = lat; }
                    if lat > max_lat { max_lat = lat; }
                }
            }
        }
    }
    (min_lon, max_lon, min_lat, max_lat)
}

fn point_in_bbox(lon: f64, lat: f64, bbox: &(f64, f64, f64, f64)) -> bool {
    let (min_lon, max_lon, min_lat, max_lat) = *bbox;
    lon >= min_lon && lon <= max_lon && lat >= min_lat && lat <= max_lat
}

fn point_in_ring(x: f64, y: f64, ring: &[Vec<f64>]) -> bool {
    let mut inside = false;
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let xi = ring[i][0];
        let yi = ring[i][1];
        let xj = ring[j][0];
        let yj = ring[j][1];

        if ((yi > y) != (yj > y))
            && (x < (xj - xi) * (y - yi) / (yj - yi) + xi)
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn point_in_polygon(lon: f64, lat: f64, polygon: &[Vec<Vec<f64>>]) -> bool {
    if polygon.is_empty() {
        return false;
    }
    let exterior = &polygon[0];
    if point_in_ring(lon, lat, exterior) {
        for hole in &polygon[1..] {
            if point_in_ring(lon, lat, hole) {
                return false;
            }
        }
        return true;
    }
    false
}

fn point_in_multipolygon(lon: f64, lat: f64, multipolygon: &[Vec<Vec<Vec<f64>>>]) -> bool {
    for polygon in multipolygon {
        if point_in_polygon(lon, lat, polygon) {
            return true;
        }
    }
    false
}

#[derive(Debug, Deserialize)]
struct AgencyRow {
    agency_id: String,
    agency_name: String,
}

#[derive(Debug, Deserialize)]
struct RouteRow {
    route_id: String,
    agency_id: String,
}

#[derive(Debug, Deserialize)]
struct TripRow {
    route_id: String,
    service_id: String,
    trip_id: String,
    shape_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StopRow {
    stop_id: String,
    stop_lat: Option<String>,
    stop_lon: Option<String>,
    parent_station: Option<String>,
    level_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StopTimeRow {
    trip_id: String,
    stop_id: String,
}

#[derive(Debug, Deserialize)]
struct ServiceRow {
    service_id: String,
}

#[derive(Debug, Deserialize)]
struct ShapeRow {
    shape_id: String,
}

#[derive(Debug, Deserialize)]
struct LevelRow {
    level_id: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Loading geojson boundaries...");
    let geojson_content = fs::read_to_string("1_sehr_hoch.geo.json")?;
    let geojson: GeoJson = serde_json::from_str(&geojson_content)?;

    let mut state_boundaries = Vec::new();
    for feature in geojson.features {
        let name = feature.properties.name;
        let clean_name = transliterate_state_name(&name);
        let coords = feature.geometry.to_multipolygon();
        let bbox = calculate_bbox(&coords);
        state_boundaries.push(StateBoundary {
            name,
            clean_name,
            coordinates: coords,
            bbox,
        });
    }
    println!("Loaded {} state boundaries.", state_boundaries.len());

    println!("Loading stops and indexing their states...");
    let stops_path = Path::new("gtfs_raw/stops.txt");
    let mut stops_reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_path(stops_path)?;

    let mut stop_map: HashMap<Arc<str>, (Option<String>, Option<Arc<str>>)> = HashMap::new();

    for result in stops_reader.deserialize::<StopRow>() {
        let row = result?;
        let stop_id: Arc<str> = Arc::from(row.stop_id);
        let parent_station: Option<Arc<str>> = row.parent_station
            .filter(|s| !s.trim().is_empty())
            .map(|s| Arc::from(s.trim()));

        let lat: Option<f64> = row.stop_lat.as_ref()
            .filter(|s| !s.trim().is_empty())
            .and_then(|s| s.trim().parse::<f64>().ok());
        let lon: Option<f64> = row.stop_lon.as_ref()
            .filter(|s| !s.trim().is_empty())
            .and_then(|s| s.trim().parse::<f64>().ok());

        let mut assigned_state: Option<String> = None;
        if let (Some(lat_val), Some(lon_val)) = (lat, lon) {
            for state in &state_boundaries {
                if point_in_bbox(lon_val, lat_val, &state.bbox) {
                    if point_in_multipolygon(lon_val, lat_val, &state.coordinates) {
                        assigned_state = Some(state.clean_name.clone());
                        break;
                    }
                }
            }
        }
        stop_map.insert(stop_id, (assigned_state, parent_station));
    }
    println!("Loaded {} stops.", stop_map.len());

    println!("Loading agencies...");
    let agency_path = Path::new("gtfs_raw/agency.txt");
    let mut agency_reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_path(agency_path)?;

    struct AgencyInfo {
        name: String,
        is_db: bool,
    }

    let mut agency_map: HashMap<String, AgencyInfo> = HashMap::new();

    for result in agency_reader.deserialize::<AgencyRow>() {
        let row = result?;
        let is_db = row.agency_name.starts_with("DB ");
        agency_map.insert(row.agency_id.clone(), AgencyInfo {
            name: row.agency_name,
            is_db,
        });
    }
    println!("Loaded {} agencies.", agency_map.len());

    println!("Loading routes...");
    let routes_path = Path::new("gtfs_raw/routes.txt");
    let mut routes_reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_path(routes_path)?;

    let mut route_agency_map: HashMap<String, String> = HashMap::new();

    for result in routes_reader.deserialize::<RouteRow>() {
        let row = result?;
        route_agency_map.insert(row.route_id, row.agency_id);
    }
    println!("Loaded {} routes.", route_agency_map.len());

    let mut agencies_list: Vec<String> = agency_map.keys().cloned().collect();
    agencies_list.sort();

    let agency_to_index: HashMap<String, u16> = agencies_list
        .iter()
        .enumerate()
        .map(|(idx, id)| (id.clone(), idx as u16))
        .collect();

    println!("Mapping trips to agency indices...");
    let trips_path = Path::new("gtfs_raw/trips.txt");
    let mut trips_reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_path(trips_path)?;

    let mut trip_agency_map: HashMap<Arc<str>, u16> = HashMap::with_capacity(2_300_000);

    for result in trips_reader.deserialize::<TripRow>() {
        let row = result?;
        if let Some(agency_id) = route_agency_map.get(&row.route_id) {
            if let Some(&agency_idx) = agency_to_index.get(agency_id) {
                let trip_id: Arc<str> = Arc::from(row.trip_id);
                trip_agency_map.insert(trip_id, agency_idx);
            }
        }
    }
    println!("Mapped {} trips.", trip_agency_map.len());

    println!("Analyzing stop times (Pass 1)...");
    let stop_times_path = Path::new("gtfs_raw/stop_times.txt");
    let mut stop_times_reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_path(stop_times_path)?;

    let mut agency_states: HashMap<u16, HashSet<Option<String>>> = HashMap::new();

    let mut count = 0;
    for result in stop_times_reader.deserialize::<StopTimeRow>() {
        let row = result?;
        if let Some(&agency_idx) = trip_agency_map.get(row.trip_id.as_str()) {
            let stop_state = stop_map.get(row.stop_id.as_str())
                .map(|(state, _)| state.clone())
                .flatten();
            agency_states.entry(agency_idx)
                .or_insert_with(HashSet::new)
                .insert(stop_state);
        }
        count += 1;
        if count % 10_000_000 == 0 {
            println!("Processed {}M stop time rows...", count / 1_000_000);
        }
    }
    println!("Processed {} total stop time rows.", count);

    let mut agency_target_folders: HashMap<u16, String> = HashMap::new();

    for (&agency_idx, states) in &agency_states {
        let agency_id = &agencies_list[agency_idx as usize];
        let agency_info = &agency_map[agency_id];
        
        let target = if agency_info.is_db {
            "deutsche_bahn".to_string()
        } else {
            if states.len() == 1 {
                if let Some(Some(state_name)) = states.iter().next() {
                    state_name.clone()
                } else {
                    "other".to_string()
                }
            } else {
                "other".to_string()
            }
        };
        agency_target_folders.insert(agency_idx, target);
    }

    for idx in 0..agencies_list.len() {
        let agency_idx = idx as u16;
        agency_target_folders.entry(agency_idx).or_insert_with(|| {
            let agency_id = &agencies_list[idx];
            let agency_info = &agency_map[agency_id];
            if agency_info.is_db {
                "deutsche_bahn".to_string()
            } else {
                "other".to_string()
            }
        });
    }

    let mut folder_counts: HashMap<String, usize> = HashMap::new();
    for folder in agency_target_folders.values() {
        *folder_counts.entry(folder.clone()).or_insert(0) += 1;
    }
    println!("Agency Routing Summary:");
    for (folder, cnt) in &folder_counts {
        println!("  {}: {} agencies", folder, cnt);
    }

    let output_dir = Path::new("gtfs_output");
    fs::create_dir_all(output_dir)?;

    let active_folders: HashSet<String> = agency_target_folders.values().cloned().collect();
    for folder in &active_folders {
        fs::create_dir_all(output_dir.join(folder))?;
    }

    println!("Writing agency.txt splits...");
    {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(Path::new("gtfs_raw/agency.txt"))?;
        let header = reader.headers()?.clone();

        let mut writers: HashMap<String, csv::Writer<BufWriter<File>>> = HashMap::new();
        for folder in &active_folders {
            let file = File::create(output_dir.join(folder).join("agency.txt"))?;
            let mut writer = csv::Writer::from_writer(BufWriter::new(file));
            writer.write_record(&header)?;
            writers.insert(folder.clone(), writer);
        }

        let mut record = csv::StringRecord::new();
        while reader.read_record(&mut record)? {
            let row: AgencyRow = record.deserialize(Some(&header))?;
            if let Some(&agency_idx) = agency_to_index.get(&row.agency_id) {
                if let Some(folder) = agency_target_folders.get(&agency_idx) {
                    if let Some(writer) = writers.get_mut(folder) {
                        writer.write_record(&record)?;
                    }
                }
            }
        }
    }

    println!("Writing routes.txt splits...");
    {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(Path::new("gtfs_raw/routes.txt"))?;
        let header = reader.headers()?.clone();

        let mut writers: HashMap<String, csv::Writer<BufWriter<File>>> = HashMap::new();
        for folder in &active_folders {
            let file = File::create(output_dir.join(folder).join("routes.txt"))?;
            let mut writer = csv::Writer::from_writer(BufWriter::new(file));
            writer.write_record(&header)?;
            writers.insert(folder.clone(), writer);
        }

        let mut record = csv::StringRecord::new();
        while reader.read_record(&mut record)? {
            let row: RouteRow = record.deserialize(Some(&header))?;
            if let Some(&agency_idx) = agency_to_index.get(&row.agency_id) {
                if let Some(folder) = agency_target_folders.get(&agency_idx) {
                    if let Some(writer) = writers.get_mut(folder) {
                        writer.write_record(&record)?;
                    }
                }
            }
        }
    }

    println!("Writing trips.txt splits...");
    let mut services_needed: HashMap<String, HashSet<Arc<str>>> = HashMap::new();
    let mut shapes_needed: HashMap<String, HashSet<Arc<str>>> = HashMap::new();
    for folder in &active_folders {
        services_needed.insert(folder.clone(), HashSet::new());
        shapes_needed.insert(folder.clone(), HashSet::new());
    }

    let mut trip_folder_map: HashMap<Arc<str>, String> = HashMap::with_capacity(2_300_000);

    {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(Path::new("gtfs_raw/trips.txt"))?;
        let header = reader.headers()?.clone();

        let mut writers: HashMap<String, csv::Writer<BufWriter<File>>> = HashMap::new();
        for folder in &active_folders {
            let file = File::create(output_dir.join(folder).join("trips.txt"))?;
            let mut writer = csv::Writer::from_writer(BufWriter::new(file));
            writer.write_record(&header)?;
            writers.insert(folder.clone(), writer);
        }

        let mut record = csv::StringRecord::new();
        while reader.read_record(&mut record)? {
            let row: TripRow = record.deserialize(Some(&header))?;
            if let Some(agency_id) = route_agency_map.get(&row.route_id) {
                if let Some(&agency_idx) = agency_to_index.get(agency_id) {
                    if let Some(folder) = agency_target_folders.get(&agency_idx) {
                        if let Some(writer) = writers.get_mut(folder) {
                            writer.write_record(&record)?;
                        }
                        let trip_id: Arc<str> = Arc::from(row.trip_id);
                        trip_folder_map.insert(trip_id, folder.clone());
                        services_needed.get_mut(folder).unwrap().insert(Arc::from(row.service_id));
                        if let Some(shape_id) = row.shape_id {
                            if !shape_id.trim().is_empty() {
                                shapes_needed.get_mut(folder).unwrap().insert(Arc::from(shape_id.trim()));
                            }
                        }
                    }
                }
            }
        }
    }

    println!("Writing stop_times.txt splits (Pass 2)...");
    let mut stops_needed: HashMap<String, HashSet<Arc<str>>> = HashMap::new();
    for folder in &active_folders {
        stops_needed.insert(folder.clone(), HashSet::new());
    }

    {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(Path::new("gtfs_raw/stop_times.txt"))?;
        let header = reader.headers()?.clone();

        let mut writers: HashMap<String, csv::Writer<BufWriter<File>>> = HashMap::new();
        for folder in &active_folders {
            let file = File::create(output_dir.join(folder).join("stop_times.txt"))?;
            let mut writer = csv::Writer::from_writer(BufWriter::new(file));
            writer.write_record(&header)?;
            writers.insert(folder.clone(), writer);
        }

        let mut record = csv::StringRecord::new();
        let mut count = 0;
        while reader.read_record(&mut record)? {
            let row: StopTimeRow = record.deserialize(Some(&header))?;
            if let Some(folder) = trip_folder_map.get(row.trip_id.as_str()) {
                if let Some(writer) = writers.get_mut(folder) {
                    writer.write_record(&record)?;
                }
                stops_needed.get_mut(folder).unwrap().insert(Arc::from(row.stop_id));
            }
            count += 1;
            if count % 10_000_000 == 0 {
                println!("Wrote {}M stop time rows...", count / 1_000_000);
            }
        }
        println!("Finished writing {} stop times rows.", count);
    }

    println!("Propagating parent stations for needed stops...");
    for (folder, stops) in &mut stops_needed {
        let mut to_add = Vec::new();
        for stop_id in stops.iter() {
            let mut curr = stop_id.clone();
            while let Some((_, parent_opt)) = stop_map.get(&curr) {
                if let Some(parent_id) = parent_opt {
                    if !stops.contains(parent_id) {
                        to_add.push(parent_id.clone());
                    }
                    curr = parent_id.clone();
                } else {
                    break;
                }
            }
        }
        for parent_id in to_add {
            stops.insert(parent_id);
        }
        println!("  {}: needs {} stops (including parents)", folder, stops.len());
    }

    println!("Writing stops.txt splits...");
    let mut levels_needed: HashMap<String, HashSet<Arc<str>>> = HashMap::new();
    for folder in &active_folders {
        levels_needed.insert(folder.clone(), HashSet::new());
    }

    {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(Path::new("gtfs_raw/stops.txt"))?;
        let header = reader.headers()?.clone();

        let mut writers: HashMap<String, csv::Writer<BufWriter<File>>> = HashMap::new();
        for folder in &active_folders {
            let file = File::create(output_dir.join(folder).join("stops.txt"))?;
            let mut writer = csv::Writer::from_writer(BufWriter::new(file));
            writer.write_record(&header)?;
            writers.insert(folder.clone(), writer);
        }

        let mut record = csv::StringRecord::new();
        while reader.read_record(&mut record)? {
            let row: StopRow = record.deserialize(Some(&header))?;
            let stop_id: Arc<str> = Arc::from(row.stop_id);
            for folder in &active_folders {
                if stops_needed[folder].contains(&stop_id) {
                    if let Some(writer) = writers.get_mut(folder) {
                        writer.write_record(&record)?;
                    }
                    if let Some(level_id) = &row.level_id {
                        if !level_id.trim().is_empty() {
                            levels_needed.get_mut(folder).unwrap().insert(Arc::from(level_id.trim()));
                        }
                    }
                }
            }
        }
    }

    println!("Writing calendar.txt splits...");
    {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(Path::new("gtfs_raw/calendar.txt"))?;
        let header = reader.headers()?.clone();

        let mut writers: HashMap<String, csv::Writer<BufWriter<File>>> = HashMap::new();
        for folder in &active_folders {
            let file = File::create(output_dir.join(folder).join("calendar.txt"))?;
            let mut writer = csv::Writer::from_writer(BufWriter::new(file));
            writer.write_record(&header)?;
            writers.insert(folder.clone(), writer);
        }

        let mut record = csv::StringRecord::new();
        while reader.read_record(&mut record)? {
            let row: ServiceRow = record.deserialize(Some(&header))?;
            let service_id_arc: Arc<str> = Arc::from(row.service_id);
            for folder in &active_folders {
                if services_needed[folder].contains(&service_id_arc) {
                    if let Some(writer) = writers.get_mut(folder) {
                        writer.write_record(&record)?;
                    }
                }
            }
        }
    }

    println!("Writing calendar_dates.txt splits...");
    {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(Path::new("gtfs_raw/calendar_dates.txt"))?;
        let header = reader.headers()?.clone();

        let mut writers: HashMap<String, csv::Writer<BufWriter<File>>> = HashMap::new();
        for folder in &active_folders {
            let file = File::create(output_dir.join(folder).join("calendar_dates.txt"))?;
            let mut writer = csv::Writer::from_writer(BufWriter::new(file));
            writer.write_record(&header)?;
            writers.insert(folder.clone(), writer);
        }

        let mut record = csv::StringRecord::new();
        while reader.read_record(&mut record)? {
            let row: ServiceRow = record.deserialize(Some(&header))?;
            let service_id_arc: Arc<str> = Arc::from(row.service_id);
            for folder in &active_folders {
                if services_needed[folder].contains(&service_id_arc) {
                    if let Some(writer) = writers.get_mut(folder) {
                        writer.write_record(&record)?;
                    }
                }
            }
        }
    }

    println!("Writing shapes.txt splits...");
    {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(Path::new("gtfs_raw/shapes.txt"))?;
        let header = reader.headers()?.clone();

        let mut writers: HashMap<String, csv::Writer<BufWriter<File>>> = HashMap::new();
        for folder in &active_folders {
            let file = File::create(output_dir.join(folder).join("shapes.txt"))?;
            let mut writer = csv::Writer::from_writer(BufWriter::new(file));
            writer.write_record(&header)?;
            writers.insert(folder.clone(), writer);
        }

        let mut record = csv::StringRecord::new();
        while reader.read_record(&mut record)? {
            let row: ShapeRow = record.deserialize(Some(&header))?;
            let shape_id: Arc<str> = Arc::from(row.shape_id.as_str());
            for folder in &active_folders {
                if shapes_needed[folder].contains(&shape_id) {
                    if let Some(writer) = writers.get_mut(folder) {
                        writer.write_record(&record)?;
                    }
                }
            }
        }
    }

    println!("Writing transfers.txt splits...");
    {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(Path::new("gtfs_raw/transfers.txt"))?;
        let header = reader.headers()?.clone();

        let mut writers: HashMap<String, csv::Writer<BufWriter<File>>> = HashMap::new();
        for folder in &active_folders {
            let file = File::create(output_dir.join(folder).join("transfers.txt"))?;
            let mut writer = csv::Writer::from_writer(BufWriter::new(file));
            writer.write_record(&header)?;
            writers.insert(folder.clone(), writer);
        }

        #[derive(Debug, Deserialize)]
        struct TransferRow {
            from_stop_id: String,
            to_stop_id: String,
        }

        let mut record = csv::StringRecord::new();
        while reader.read_record(&mut record)? {
            let row: TransferRow = record.deserialize(Some(&header))?;
            let from_stop: Arc<str> = Arc::from(row.from_stop_id.as_str());
            let to_stop: Arc<str> = Arc::from(row.to_stop_id.as_str());
            for folder in &active_folders {
                if stops_needed[folder].contains(&from_stop) && stops_needed[folder].contains(&to_stop) {
                    if let Some(writer) = writers.get_mut(folder) {
                        writer.write_record(&record)?;
                    }
                }
            }
        }
    }

    println!("Writing pathways.txt splits...");
    {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(Path::new("gtfs_raw/pathways.txt"))?;
        let header = reader.headers()?.clone();

        let mut writers: HashMap<String, csv::Writer<BufWriter<File>>> = HashMap::new();
        for folder in &active_folders {
            let file = File::create(output_dir.join(folder).join("pathways.txt"))?;
            let mut writer = csv::Writer::from_writer(BufWriter::new(file));
            writer.write_record(&header)?;
            writers.insert(folder.clone(), writer);
        }

        #[derive(Debug, Deserialize)]
        struct PathwayRow {
            from_stop_id: String,
            to_stop_id: String,
        }

        let mut record = csv::StringRecord::new();
        while reader.read_record(&mut record)? {
            let row: PathwayRow = record.deserialize(Some(&header))?;
            let from_stop: Arc<str> = Arc::from(row.from_stop_id.as_str());
            let to_stop: Arc<str> = Arc::from(row.to_stop_id.as_str());
            for folder in &active_folders {
                if stops_needed[folder].contains(&from_stop) && stops_needed[folder].contains(&to_stop) {
                    if let Some(writer) = writers.get_mut(folder) {
                        writer.write_record(&record)?;
                    }
                }
            }
        }
    }

    println!("Writing levels.txt splits...");
    {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(Path::new("gtfs_raw/levels.txt"))?;
        let header = reader.headers()?.clone();

        let mut writers: HashMap<String, csv::Writer<BufWriter<File>>> = HashMap::new();
        for folder in &active_folders {
            let file = File::create(output_dir.join(folder).join("levels.txt"))?;
            let mut writer = csv::Writer::from_writer(BufWriter::new(file));
            writer.write_record(&header)?;
            writers.insert(folder.clone(), writer);
        }

        let mut record = csv::StringRecord::new();
        while reader.read_record(&mut record)? {
            let row: LevelRow = record.deserialize(Some(&header))?;
            let level_id: Arc<str> = Arc::from(row.level_id.as_str());
            for folder in &active_folders {
                if levels_needed[folder].contains(&level_id) {
                    if let Some(writer) = writers.get_mut(folder) {
                        writer.write_record(&record)?;
                    }
                }
            }
        }
    }

    println!("Writing frequencies.txt splits...");
    {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(Path::new("gtfs_raw/frequencies.txt"))?;
        let header = reader.headers()?.clone();

        let mut writers: HashMap<String, csv::Writer<BufWriter<File>>> = HashMap::new();
        for folder in &active_folders {
            let file = File::create(output_dir.join(folder).join("frequencies.txt"))?;
            let mut writer = csv::Writer::from_writer(BufWriter::new(file));
            writer.write_record(&header)?;
            writers.insert(folder.clone(), writer);
        }

        #[derive(Debug, Deserialize)]
        struct FrequencyRow {
            trip_id: String,
        }

        let mut record = csv::StringRecord::new();
        while reader.read_record(&mut record)? {
            let row: FrequencyRow = record.deserialize(Some(&header))?;
            if let Some(folder) = trip_folder_map.get(row.trip_id.as_str()) {
                if let Some(writer) = writers.get_mut(folder) {
                    writer.write_record(&record)?;
                }
            }
        }
    }

    let feed_info_path = Path::new("gtfs_raw/feed_info.txt");
    if feed_info_path.exists() {
        println!("Writing feed_info.txt splits...");
        let content = fs::read(feed_info_path)?;
        for folder in &active_folders {
            fs::write(output_dir.join(folder).join("feed_info.txt"), &content)?;
        }
    }

    println!("All splits successfully written.");
    Ok(())
}
